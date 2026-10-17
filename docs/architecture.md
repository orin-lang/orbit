# Orbit architecture

Orbit runs many independent WebAssembly applications ("isolates") in one host process, sharing one compilation engine,
one code cache and host-managed infrastructure, while each instance keeps its own memory, state and authority. It is
**experimental**: see [security.md](security.md) before trusting it with anything.

## Layers

```
            ┌───────────────────────────── orbit-cli (orbit run / inspect) ─────────────────────────────┐
            │                         embedding API (a future orchestrator uses the same)               │
            └───────────────────────────────────────────┬───────────────────────────────────────────────┘
                                                        │
 orbit-runtime ─────────────────────────────────────────┼──────────────────────────────────────────────────
   Runtime      admission control (concurrency semaphore, memory budget), artifact cache, registry
   OrbitEngine  one shared wasmtime::Engine (Cranelift, epoch interruption, async, Memory64, optional pooling, optional fuel metering)
   Artifact     validated + precompiled module, content hash, required capabilities, manifest
   Isolate      one Store + instance: lifecycle state machine, limits, metrics, per-capability state
   Capability   trait: id + link into a Linker; guest-memory helpers with checked bounds
                                                        │
 orbit-caps ────────────────────────────────────────────┼──────────────────────────────────────────────────
   contracts    orbit:log/v1  orbit:clock/v1  orbit:random/v1  orbit:kv/v1  wasi_p1/v1 (wasi_snapshot_preview1)
   adapters     LogSink (stderr, memory)   KvStore (in-memory, tenant-scoped)   ... later: Postgres pool, Kafka, S3
                                                        │
 orbit-abi ─────────────────────────────────────────────┴──────────────────────────────────────────────────
   ABI version, capability ids, function signatures, status codes, manifest format (no wasmtime dependency)
```

`orbit-abi` has no dependency on Wasmtime so a compiler, a linter or an orchestrator can use the contract without
pulling in an engine.

## The life of an application

1. **Load** (`Runtime::load`): bytes are hashed; a cache hit returns the compiled `Artifact`. Otherwise Wasmtime
   validates and compiles, and Orbit checks every import against the ABI (unknown modules, non-function imports and
   signature mismatches are rejected) and reads the optional `orbit.manifest`.
2. **Spawn** (`Runtime::spawn_isolate`): admission control reserves the isolate's memory limit from the runtime's
   budget; the grant set is checked against the artifact's requirements; a `Linker` with **only** the granted
   capabilities is built; a fresh `Store` gets the limiter, the epoch deadline and, on a metered engine, the fuel budget. `Limits::fuel` on an unmetered engine is refused (`InstantiateError::FuelMeteringDisabled`). State: `Created`.
3. **Init**: `_initialize`, then `orbit_init`. Success: `Initialized` -> `Idle`. A trap or a non-zero result: `Failed`.
4. **Invoke**: `Idle` -> `Running` (-> `WaitingForIo` while an async host call is pending) -> `Idle`. Fuel and the
   wall-clock deadline are reset per invocation. A trap, fuel exhaustion or a deadline leaves the isolate `Failed`;
   it can only be terminated. The one exception is `Isolate::post_mortem_i32`, which after a *trap* may read an
   `() -> i32` export (the Orin exit status, [host-abi.md](host-abi.md) section 5) without leaving `Failed`.
5. **Terminate**: `Stopping` -> `Terminated`; the store is dropped (linear memory, tables, handles and per-capability
   state with it) and the memory reservation is returned to the budget.

A dormant application is just its `Artifact` (shared compiled code) plus configuration: no store, no heap, no thread.

## Execution and scheduling

* Isolates run as **async tasks** on a Tokio runtime with a small number of worker threads; an isolate never owns a
  thread. Wasmtime's async support runs guest code on a separate fiber stack so host calls can await.
* **Epoch interruption** is the time slice: a ticker thread increments the engine epoch; at every deadline the guest
  yields back to the executor (`epoch_deadline_async_yield_and_update`), so one busy isolate cannot starve the others
  on a worker thread. An absolute per-invocation wall-clock budget is enforced at the same check points.
* **Fuel** is the deterministic CPU budget per invocation (instructions, approximately). It is **opt-in**
  (`EngineConfig::fuel_metering`, default off, [ADR-0008](adr/0008-fuel-metering-is-opt-in.md)) because metered code
  runs about 1.2x to 1.8x slower on compute-bound work. When on, it is *measured* (the store reports exact consumed
  fuel) but it is not CPU time. When off, runaway code is bounded by the wall-clock deadline (`Limits::timeout`,
  epoch based, tick-granular, not deterministic); fairness yields work in both modes.
* **Admission control**: at most N invocations run at once (semaphore); the sum of memory limits of live isolates
  never exceeds the runtime's memory budget; above that `spawn_isolate` fails fast instead of overcommitting.

## Capabilities: contract versus adapter

A capability is two things: the **contract** (import module `orbit:kv/v1`, its functions and their semantics, in
`orbit-abi` and [host-abi.md](host-abi.md)) and an **adapter** behind it (`KvStore` in memory today; a shared Postgres
or Redis pool later). Adapters are shared by all isolates of the process, so there is one connection pool per host,
not per isolate; ownership and tenant scoping are enforced in the capability layer, not by trusting the adapter.

Authority has exactly one source: the **grant** the embedder passes to `spawn_isolate` (the CLI's `--grant`). The
manifest and the import list say what a module *needs*; they never give it anything ([ADR-0003](adr/0003-deny-by-default-linking-and-manifests.md)).

`wasi_p1/v1` is the same kind of object with a different import module name: a restricted WASI preview 1 shim whose
stdout/stderr feed the log sink, whose clock and random functions are only live when `clock/v1` / `random/v1` are
granted too (it reads the clock capability's time source and resolution rather than keeping its own), and whose
`proc_exit` is a host error (`GuestExit`) the isolate turns into an exit code. To flush its line buffers when an
invocation ends, the `Capability` trait has an `end_invocation` hook next to `begin_invocation`, and `InstanceInfo`
carries the isolate's grants so a capability can see which others were granted
([ADR-0007](adr/0007-restricted-wasi-preview1-shim.md)).

Handles (KV namespaces, later files, sockets, streams) live in a per-instance handle table: an index plus a generation,
never a host pointer. A handle from another instance or a closed handle is `ERR_BAD_HANDLE`; terminating an instance
drops its table.

## Resource accounting

| Metric | Kind |
|---|---|
| fuel consumed per invocation | measured (exact Wasmtime fuel) on a metered engine; **not measured** (`None`) otherwise |
| wall time per invocation, instantiation latency | measured (monotonic clock) |
| linear memory current / peak bytes | measured (limiter) |
| host calls per capability, bytes logged | measured (counters) |
| memory reserved (admission) | configured upper bound, not usage |
| CPU time | not measured in milestone 1 (fuel is the proxy; per-thread CPU clocks are on the roadmap) |
| host-side memory of adapters | not measured in milestone 1 |

## Decisions

See [adr/](adr/): Wasmtime (0001), a core-wasm ABI before the Component Model (0002), deny-by-default linking and
manifests as description (0003), 64-bit pointers in the ABI (0004), async isolates with epoch time slices (0005),
shared adapters with per-instance handles (0006), the restricted WASI preview 1 shim (0007).
