#!/usr/bin/env bash
# Builds the WebAssembly tasks, validates them and checks the golden copies byte for byte
# (mandelbrot, toy-trainer and the optdigits dataset).
# Usage: scripts/build-tasks.sh [--update-golden]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

UPDATE_GOLDEN=0
case "${1:-}" in
  "") ;;
  --update-golden) UPDATE_GOLDEN=1 ;;
  *) echo "usage: scripts/build-tasks.sh [--update-golden]" >&2; exit 2 ;;
esac

TASKS=(mandelbrot skycmd-arena toy-trainer)

CARGO_HOME_DIR="${CARGO_HOME:-$HOME/.cargo}"
# Remap absolute paths so the module bytes do not depend on where the repo lives.
# trim-paths is not stable in this cargo; these flags are merged with tasks/.cargo/config.toml.
REMAP="target.wasm32-unknown-unknown.rustflags=[\"--remap-path-prefix=$ROOT=/nuee\",\"--remap-path-prefix=$CARGO_HOME_DIR=/cargo\"]"

OUT_DIR="tasks/target/wasm32-unknown-unknown/release"
mkdir -p web/tasks target/tasks

for task in "${TASKS[@]}"; do
  (cd tasks && cargo build --release --locked --config "$REMAP" -p "$task")
done

VALIDATE=(cargo run -q --locked -p nuee-proto --example validate --)
index="["
sep=""
for task in "${TASKS[@]}"; do
  module="$OUT_DIR/${task//-/_}.wasm"
  if grep -qF -e "$ROOT" -e "$HOME" "$module"; then
    echo "$task: module contains an absolute path" >&2
    exit 1
  fi
  if ! line="$("${VALIDATE[@]}" "$module")"; then
    echo "$line" >&2
    exit 1
  fi
  read -r _ status hash_kv size_kv mem_kv <<<"$line"
  if [[ "$status" != ok ]]; then
    echo "$line" >&2
    exit 1
  fi
  install -m 0644 "$module" "web/tasks/$task.wasm"
  install -m 0644 "$module" "target/tasks/$task.wasm"
  index+="$sep{\"name\":\"$task\",\"hash\":\"${hash_kv#hash=}\",\"size\":${size_kv#size=},\"max_memory\":${mem_kv#max_memory=}}"
  sep=","
  echo "$task ok ${hash_kv} ${size_kv} ${mem_kv}"
done
printf '%s]\n' "$index" >web/tasks/index.json
# The GPU twin of the mandelbrot task is plain WGSL text, served as is.
install -m 0644 tasks/mandelbrot/mandelbrot.wgsl web/tasks/mandelbrot.wgsl

# The toy trainer's dataset asset (layout documented in tasks/toy-trainer/src/lib.rs):
# b"OPTD", u32 LE n_train, u32 LE n_test, then 64 pixel bytes + 1 label byte per sample.
optd="tasks/toy-trainer/data"
perl -e '
  my @rows;
  for my $f (@ARGV) {
    open(my $fh, "<", $f) or die "$f: $!";
    my @s;
    while (<$fh>) {
      s/\r?\n$//;
      next if $_ eq "";
      my @v = split /,/;
      die "$f: bad row\n" unless @v == 65 && !grep { !/^\d+$/ } @v;
      die "$f: value out of range\n" if grep { $_ > 16 } @v[0..63] or $v[64] > 9;
      push @s, pack("C65", @v);
    }
    push @rows, \@s;
  }
  binmode STDOUT;
  print "OPTD", pack("V V", scalar @{$rows[0]}, scalar @{$rows[1]}), @{$rows[0]}, @{$rows[1]};
' "$optd/optdigits.tra" "$optd/optdigits.tes" >target/tasks/optdigits.bin
chmod 0644 target/tasks/optdigits.bin
install -m 0644 target/tasks/optdigits.bin web/tasks/optdigits.bin
echo "optdigits.bin ok size=$(wc -c <target/tasks/optdigits.bin | tr -d ' ')"

# Goldens: the CLI embeds these bytes, so a rebuild anywhere (macOS, Linux CI, another checkout
# path) must reproduce them exactly. skycmd-arena has no golden: its skycmd dependencies use
# serde_derive, a proc-macro built for the host, and cargo mixes the host triple into the metadata
# (hence the symbol names and function order) of every crate that depends on a host crate, so its
# bytes differ between host platforms on stable cargo with a pinned skycmd.
GOLDENS=(mandelbrot.wasm toy-trainer.wasm optdigits.bin)
mkdir -p tasks/golden
for name in "${GOLDENS[@]}"; do
  golden="tasks/golden/$name"
  if [[ "$UPDATE_GOLDEN" == 1 ]]; then
    install -m 0644 "target/tasks/$name" "$golden"
    echo "golden updated: $golden"
  fi
  if [[ ! -f "$golden" ]]; then
    echo "missing $golden (run scripts/build-tasks.sh --update-golden once)" >&2
    exit 1
  fi
  if ! cmp -s "target/tasks/$name" "$golden"; then
    echo "$name golden mismatch: target/tasks/$name differs from $golden" >&2
    exit 1
  fi
  echo "golden $name matches"
done
