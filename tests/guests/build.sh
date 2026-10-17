#!/usr/bin/env bash
# Rebuilds the guest fixtures that are committed under crates/orbit-caps/tests/fixtures/.
#
#   wasi_hello.wasm  Rust, wasm32-wasip1          needs: rustup target add wasm32-wasip1
#   wasi_c.wasm      C, clang + wasi-libc         needs: clang (any recent) and the Rust target
#
# The C fixture links Rust's self-contained wasi-libc (libc.a, crt1-command.o) with rust-lld,
# because a wasi sysroot with headers is not assumed to be installed. It is skipped when clang
# is missing.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
out="$root/crates/orbit-caps/tests/fixtures"
mkdir -p "$out"

cargo build --release --target wasm32-wasip1 --manifest-path "$here/wasi-hello/Cargo.toml"
install -m 0644 "$here/wasi-hello/target/wasm32-wasip1/release/wasi-hello.wasm" "$out/wasi_hello.wasm"
echo "wrote $out/wasi_hello.wasm ($(wc -c < "$out/wasi_hello.wasm") bytes)"

if command -v clang >/dev/null 2>&1; then
  sysroot="$(rustc --print sysroot)"
  libc_dir="$sysroot/lib/rustlib/wasm32-wasip1/lib/self-contained"
  host="$(rustc -vV | sed -n 's/^host: //p')"
  lld="$sysroot/lib/rustlib/$host/bin/rust-lld"
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  clang --target=wasm32-wasip1 -O2 -nostdinc -ffreestanding -c "$here/wasi-c/main.c" -o "$tmp/main.o"
  "$lld" -flavor wasm "$libc_dir/crt1-command.o" "$tmp/main.o" "$libc_dir/libc.a" \
    -o "$out/wasi_c.wasm" --export-memory --gc-sections --strip-all
  chmod 0644 "$out/wasi_c.wasm"
  echo "wrote $out/wasi_c.wasm ($(wc -c < "$out/wasi_c.wasm") bytes)"
else
  echo "clang not found: skipping the C fixture (the committed one is kept)"
fi
