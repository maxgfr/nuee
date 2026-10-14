#!/usr/bin/env bash
# Builds the browser package (crates/nuee-web) into web/pkg/<version>/ and points
# web/pkg/current.json at it. The page loads the versioned files named there.
# Usage: scripts/build-web.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [ "$#" -ne 0 ]; then
  echo "usage: scripts/build-web.sh" >&2
  exit 2
fi

for tool in cargo wasm-pack wasm-opt node; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "build-web: $tool is required" >&2
    exit 1
  fi
done

VERSION="$(cargo metadata --no-deps --format-version 1 --locked |
  node -e '
    let s = "";
    process.stdin.on("data", (d) => (s += d));
    process.stdin.on("end", () => {
      const pkg = JSON.parse(s).packages.find((p) => p.name === "nuee-web");
      if (!pkg) { console.error("nuee-web not found in cargo metadata"); process.exit(1); }
      process.stdout.write(pkg.version);
    });
  ')"

# ring compiles C for wasm32; the system `ar` on macOS cannot index wasm objects, so the
# archive is built with the llvm-ar of the toolchain (component llvm-tools).
SYSROOT="$(rustc --print sysroot)"
HOST="$(rustc -vV | sed -n 's/^host: //p')"
LLVM_AR="$SYSROOT/lib/rustlib/$HOST/bin/llvm-ar"
if [ ! -x "$LLVM_AR" ]; then
  echo "build-web: $LLVM_AR missing; run: rustup component add llvm-tools" >&2
  exit 1
fi
export AR_wasm32_unknown_unknown="$LLVM_AR"

OUT="$ROOT/web/pkg/$VERSION"
rm -rf "$OUT"
mkdir -p "$OUT"

# wasm-opt is disabled in crates/nuee-web/Cargo.toml so that it runs here with these flags.
wasm-pack build crates/nuee-web --target web --release --out-dir "$OUT" --out-name nuee_web

WASM="$OUT/nuee_web_bg.wasm"
# Same flags as skycmd (sky-infer/Cargo.toml), plus the features rustc enables by default
# for wasm32-unknown-unknown.
wasm-opt -O3 \
  --enable-simd \
  --enable-bulk-memory \
  --enable-nontrapping-float-to-int \
  --enable-sign-ext \
  --enable-mutable-globals \
  --enable-reference-types \
  --enable-multivalue \
  "$WASM" -o "$WASM.opt"
mv "$WASM.opt" "$WASM"

# wasm-pack writes these for npm publishing; the page does not use them.
rm -f "$OUT/package.json" "$OUT/.gitignore" "$OUT/README.md"

# Transformers.js and its ONNX Runtime wasm, checked against web/vendor/transformers/VENDOR.json.
node "$ROOT/web/vendor/transformers/fetch.mjs"

printf '{"version": "%s", "path": "pkg/%s/"}\n' "$VERSION" "$VERSION" >"$ROOT/web/pkg/current.json"

SIZE="$(wc -c <"$WASM" | tr -d ' ')"
echo "build-web: web/pkg/$VERSION/nuee_web_bg.wasm is $SIZE bytes"
