#!/usr/bin/env bash
# Rebuilds the Orin guest fixtures under crates/orbit-cli/tests/fixtures/orin/ (committed, so CI needs no Orin toolchain).
#
#   ORIN_DIR=/path/to/orin-lang tests/guests/orin/build.sh
#
# ORIN_DIR is an Orin checkout with the compiler bootstrapped (./tools/bootstrap.sh; tools/orin builds build/dev/orin on first use).
# Needs clang with the WebAssembly backend and wasm-ld (package lld). Each program is built twice (release): <name>_32.wasm
# (wasm32-orbit) and <name>_64.wasm (wasm64-orbit). Also builds the native binaries and writes their stdout to
# <name>.expected, which the tests compare against. The Orin commit goes to VERSION.
set -euo pipefail
: "${ORIN_DIR:?set ORIN_DIR to an Orin checkout}"
here="$(cd "$(dirname "$0")" && pwd)"
out="$(cd "$here/../../.." && pwd)/crates/orbit-cli/tests/fixtures/orin"
mkdir -p "$out"
export CC="${CC:-clang}"
orin="$ORIN_DIR/tools/orin"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
for dir in "$here"/*/; do
  name="$(basename "$dir")"
  cp -r "$dir" "$work/$name"
  for w in 32 64; do
    (cd "$work/$name" && "$orin" build --release --target=wasm$w-orbit -o "$out/${name}_$w.wasm") >/dev/null
    echo "wrote ${name}_$w.wasm ($(wc -c < "$out/${name}_$w.wasm") bytes)"
  done
  (cd "$work/$name" && "$orin" build --release -o "$work/$name.native") >/dev/null
  # The expected output is the native program's stdout; the exit status is checked by the tests.
  "$work/$name.native" > "$out/$name.expected" 2>/dev/null || true
done
rm -f "$out"/*.fingerprint
{
  echo "orin-lang commit: $(git -C "$ORIN_DIR" rev-parse HEAD)"
  echo "command: orin build --release --target=wasm{32,64}-orbit (CC=$CC)"
  echo "built: $(date -u +%Y-%m-%d)"
} > "$out/VERSION"
