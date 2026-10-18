#!/usr/bin/env bash
# Rebuilds the thread guest fixtures committed under crates/orbit-runtime/tests/fixtures/threads/.
# Needs clang with the WebAssembly backend and wasm-ld (package lld). Freestanding: no libc.
#
# Link flags are the contract for a threaded Orbit guest: a shared memory imported as env.memory
# (--shared-memory --import-memory), with the 4 GiB maximum wasm32 allows (Orbit lowers it to
# the isolate's memory limit), and no entry point.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
out="$(cd "$here/../../.." && pwd)/crates/orbit-runtime/tests/fixtures/threads"
mkdir -p "$out"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
for name in threads net_threads; do
  clang --target=wasm32 -O2 -nostdlib -ffreestanding -matomics -mbulk-memory -c "$here/$name.c" -o "$tmp/$name.o"
  wasm-ld --no-entry --shared-memory --import-memory --max-memory=4294967296 \
    --export=orin_thread_start --gc-sections --strip-all "$tmp/$name.o" -o "$out/$name.wasm"
  chmod 0644 "$out/$name.wasm"
  echo "wrote $out/$name.wasm ($(wc -c < "$out/$name.wasm") bytes)"
done
