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

## Milestone 4 — network and event dispatch
Revised: unchanged Orin `std:net` / `std:http` code must run, so Orbit offers the L0 network layer rather than an event
ABI ([ADR-0010](adr/0010-network-capability-mirrors-the-orin-l0-tcp-layer.md)).
1. ~~`orbit:net/v1`~~ **done**: TCP listen/accept/connect/recv/send, DNS, wake sources, scope grants, async waits.
   `orbit run app.wasm --listen 127.0.0.1:8080` serves with a guest-written server.
2. `orbit:thread/v1` (several guest threads of one isolate on a shared memory) so one isolate serves connections
   concurrently; the network state is already per isolate, not per store.
3. TLS and UDP; `poll` over several sockets; outgoing connection pool behind `connect`.
4. An `Invocation` abstraction and timers (`orbit:timer/v1`) for batch/message workloads (the `orbit_alloc` export stays
   reserved for it).

## Milestone 5 — infrastructure adapters
Filesystem (in-memory, restricted local subtree, temp), object storage (S3-compatible), PostgreSQL (one shared pool
per host, per-tenant roles), Kafka and other brokers (host-managed consumers, the isolate is invoked per batch).

## Milestone 6 — cluster integration
A small control-plane protocol (load artifact, bind capabilities, spawn/terminate, metrics, health) over the existing
embedding API, so an orchestrator can run Orbit on one machine or many. No coupling to Kubernetes or a cloud.

## Continuous
Fuzzing, CPU-time accounting, process-level sandboxing (see [security.md](security.md)), benchmark tracking in CI,
a WIT description of the ABI (ADR-0002).
