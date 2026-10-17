# ADR-0001: Wasmtime as the execution engine

Status: accepted (2026-10-17)

## Context
Orbit must run untrusted WebAssembly with memory isolation, resource limits, interruption and async host calls, at
high density. Writing an interpreter or JIT is out of scope.

## Options
* **Wasmtime** (Bytecode Alliance): Cranelift AOT/JIT, fuel and epoch interruption, `ResourceLimiter`, async host
  functions on fibers, pooling allocator with copy-on-write memory images, Memory64, Component Model, a security
  process with advisories and continuous fuzzing.
* **Wasmer**: comparable feature set, smaller security/fuzzing investment, the Component Model lags.
* **WAMR / wasm3** (interpreters, small footprint): far slower steady state; good for embedded, not for servers.
* **V8**: brings the JS layer the vision excludes.

## Decision
Wasmtime, pinned to one major version per release (49 today), with Cranelift.

## Consequences
Fuel, epochs, limiter and pooling come for free and are tested upstream. Wasmtime's major version cadence (monthly)
means regular upgrades; the engine is wrapped by `OrbitEngine` so the rest of Orbit sees a small surface. Wasmtime is
part of the trusted computing base ([security.md](../security.md)).
