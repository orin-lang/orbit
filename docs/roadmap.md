# Orbit roadmap

Ordered by priority. Each item lands as its own PR with tests; an item is not "done" without benchmarks where it
touches performance.

## Milestone 1 — the foundation (this PR)
Workspace, shared Wasmtime engine, validated and cached artifacts, isolates with an explicit lifecycle, versioned host
ABI with four capabilities (log, clock, random, in-memory KV), deny-by-default grants, memory/fuel/deadline limits,
admission control, metrics, `orbit` CLI, integration tests, Criterion benchmarks, CI, ADRs.

## Milestone 2 — make Orin run
1. ~~Restricted WASI preview 1 shim~~ **done** (`wasi_p1/v1`, [ADR-0007](adr/0007-restricted-wasi-preview1-shim.md)):
   `fd_write` -> log sink, `proc_exit`, `args`/`environ` from the grant, clock and random only with `clock/v1` /
   `random/v1`; command modules run via `_start`; tested with real Rust and C (clang + wasi-libc) builds. It is
   wasm32-only: `wasm64-wasip1` output cannot use it (WASI preview 1 has 32-bit pointers), so a wasm64 Orin build
   keeps using the `orbit:*` capabilities.
2. Work with the Orin project on an `orbit` L0 and the `orbit.manifest` section ([orin-integration.md](orin-integration.md) §3).
3. `orbit_alloc` and byte payloads for invocations (host-to-guest buffers).

## Milestone 3 — density and warm reuse
1. Wasmtime pooling allocator tuned per deployment class; copy-on-write memory images (`memory-init-cow`).
2. Instance pools: pre-initialised idle instances per artifact, reset policy (fresh store per request vs reuse with
   state wiped), configurable per deployment.
3. Precompiled artifact store on disk (`Engine::precompile_module`, versioned by engine config hash).
4. Scale-to-zero: evict idle isolates by LRU under memory pressure.

## Milestone 4 — event dispatch
1. An `Invocation` abstraction: HTTP request, message, timer, batch chunk; one dispatcher API for all.
2. `orbit:http/v1` (incoming handler + outgoing client behind a host-managed connection pool).
3. Timers and scheduling (`orbit:timer/v1`), streaming bodies with zero-copy where Wasmtime allows.

## Milestone 5 — infrastructure adapters
Filesystem (in-memory, restricted local subtree, temp), object storage (S3-compatible), PostgreSQL (one shared pool
per host, per-tenant roles), Kafka and other brokers (host-managed consumers, the isolate is invoked per batch).

## Milestone 6 — cluster integration
A small control-plane protocol (load artifact, bind capabilities, spawn/terminate, metrics, health) over the existing
embedding API, so an orchestrator can run Orbit on one machine or many. No coupling to Kubernetes or a cloud.

## Continuous
Fuzzing, CPU-time accounting, process-level sandboxing (see [security.md](security.md)), benchmark tracking in CI,
a WIT description of the ABI (ADR-0002).
