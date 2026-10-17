# ADR-0004: 64-bit pointers and lengths in the ABI

Status: accepted (2026-10-17)

## Context
Orin's IR has a 64-bit `usize`, and its roadmap names `wasm64` as the first WebAssembly target. Most other toolchains
produce `wasm32`.

## Decision
Every pointer and length in the ABI is an `i64`. The host accepts both 32-bit and 64-bit memories and checks the pair
against the memory's current size with overflow-checked `u64` arithmetic.

## Consequences
One ABI for both widths; wasm32 guests pay one zero-extension per argument (negligible next to a host call). A
negative `i64` from a buggy guest is just an out-of-range value (`ERR_INVALID`). Memory64 code is currently somewhat
slower in engines because bounds checks cannot use guard pages as effectively; Orbit does not force it, it only allows it.
