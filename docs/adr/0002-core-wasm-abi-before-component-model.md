# ADR-0002: A core-WebAssembly host ABI before the Component Model

Status: accepted (2026-10-17)

## Context
The host interface could be a Component Model world described in WIT (canonical ABI, `wasmtime::component`) or a
plain set of core-wasm imports with integer arguments.

## Considerations
* Orin has no WebAssembly backend yet; its planned path is LLVM -> `clang --target=wasm64-wasip1`, which produces core
  modules. Producing components needs an extra step (`wasm-tools component new` plus an adapter) and the canonical
  ABI's lifting/lowering in the guest runtime.
* The Component Model's canonical ABI is defined for 32-bit memories; Orin's first target is Memory64 (ADR-0004).
* High-throughput host calls (logging, KV, later batch I/O) want a thin boundary: a core import is one indirect call
  plus bounds checks, with no lifting of strings or lists into fresh allocations.
* WIT gives a language-neutral, versioned interface description and resource types with ownership. Those are valuable.

## Decision
ABI v1 is core-wasm: imports `orbit:<capability>/v<major>`, `i64` pointers and lengths, `i32` status codes
([host-abi.md](../host-abi.md)). Its rules mirror the Component Model's where they matter (per-interface versions,
handles owned by the instance, no shared memory between guest and host), so a WIT description of the same interfaces
can be added later and served side by side by the same capability implementations.

## Consequences
Simple to target from any compiler, fast, works with Memory64 today. We give up generated bindings and the
canonical ABI's string/list handling; Orin's L0 implements the few calls by hand. Revisit when Orin emits components or
when Memory64 is supported by the canonical ABI.
