# ADR-0009: Instance pools are fresh-per-acquire by default

Status: accepted (2026-10-18)

## Context
Creating an isolate costs a few microseconds (instantiation, see [benchmarks.md](../benchmarks.md)) plus whatever the
guest's `_initialize` / `orbit_init` does, which for a real program (an Orin runtime start-up, a language runtime) can
be far more. Request-driven workloads (milestone 4) want that work done *before* the request arrives. The obvious
design, a pool of idle isolates that requests borrow and return, has an equally obvious hazard: an isolate that
served request A still holds A's linear memory, mutable globals, tables and capability handles when it serves request
B. For a multi-tenant host that is a data-leak and a confused-deputy risk, and it is exactly the thing isolates were
supposed to rule out.

## Decision
1. `Runtime::pool(artifact, tenant, grants, limits, PoolConfig) -> InstancePool`. A pool is bound to one
   *(artifact, tenant, grants, limits)* tuple at construction. `acquire()` takes no tenant, grants or limits, and a
   `PooledIsolate` guard can only return to the pool it came from. Cross-tenant and cross-grant sharing is therefore
   impossible **by construction**, not by a check that could be forgotten. `Runtime::pool` never merges pools; the
   embedder owns the map from "application" to pool.
2. `ReusePolicy::FreshPerAcquire` is the **default**. Every acquire hands out an isolate that has been initialised and
   never used by a caller; releasing it terminates it. Isolation between requests is exactly that of
   `spawn_isolate`, and the pool only moves the instantiate-and-init latency off the request path (prewarm,
   `min_idle`, background refill).
3. `ReusePolicy::Reuse { max_uses }` is opt-in. A healthy isolate (`Idle`, or untouched `Initialized`) goes back to
   the pool and is handed out again, at most `max_uses` times. This keeps **everything the guest left behind** (memory,
   globals, tables, open handles) for the next request **of the same tenant**. Only per-invocation budgets (fuel,
   deadline, log bytes) are reset. We do not try to "wipe" an instance: there is no sound way to reset arbitrary guest
   state short of re-instantiating it, and a half-reset mode would be a false promise.
4. Never reused, under either policy: `Failed` (trap, fuel, deadline, memory limit, non-zero exit, failed init),
   `Terminated` (`proc_exit(0)`), and `Running` (the caller cancelled an invocation mid-call). `discard()` forces
   termination.
5. Pooled idle isolates keep their `Limits::max_memory_bytes` **reservation** in the admission budget. That is the honest
   cost of warm isolates and it is reported (`PoolStats::idle_reserved_bytes`). To keep warm pools from starving new
   work, a spawn that fails admission calls `Runtime::reclaim_idle`, which terminates idle pooled isolates, longest
   idle first, across all pools of the runtime, and retries once. Checked-out isolates are never evicted.
6. Scale to zero: a per-pool background task (weak reference, aborted with the last pool handle) terminates isolates
   idle longer than `PoolConfig::idle_timeout` down to `min_idle`; `min_idle = 0` lets an unused pool disappear
   completely, and the next acquire creates an isolate on demand.
7. The pooling allocator knobs are part of `PoolingConfig` (max instances, memory slot size, table elements, tables per
   instance, keep-resident bytes for memory, tables and stacks) with `density` and `latency` presets.

## Consequences
* With the default policy a pool is a latency optimisation with no semantic effect. Switching to `Reuse` is a
  deliberate, per-deployment decision that the embedder must make knowing the guest keeps state; it is documented on
  the type and in `docs/security.md`-relevant places, and `max_uses` bounds how long a leak or a growing heap can live.
* Under `Reuse`, an isolate's own state may make requests of one tenant observable to each other. Orbit does not
  attempt to prevent that. Do not use it when requests of one tenant act for different end users unless the guest is
  written for it.
* Idle isolates cost memory budget and pooling-allocator slots (`PoolingConfig::max_instances` counts idle ones).
  Operators size `max_idle` / `min_idle` against that, and `reclaim_idle` is the safety valve, not a substitute.
* `min_idle` is best effort: reclaim-evicted isolates are not refilled until the next acquire or release (so the refill
  task cannot steal the memory the reclaim just freed).
* A pool's `acquire` waits when `max_total` isolates are checked out. Callers bound the wait with
  `tokio::time::timeout` (the future is cancel-safe) or use `try_acquire`.
* No precompiled-code disk cache (milestone 3, item 3) is included. `Module::deserialize` is `unsafe` in Wasmtime and
  the workspace is `#![forbid(unsafe_code)]`; it needs a separately audited unsafe boundary (content-addressed, signed or
  trusted storage, engine-config-hash versioning) and stays future work.

## Alternatives considered
* **Reuse by default** (what many FaaS runtimes do): fastest, but a silent cross-request leak. Rejected as a default.
* **Reset by re-instantiating inside the pool** under `Reuse`: that is `FreshPerAcquire` with extra steps.
* **A shared per-artifact pool keyed by tenant at acquire time**: one forgotten key or a wrong comparison hands out
  another tenant's isolate. Rejected in favour of one pool object per tuple.
* **Evict by timer only**: leaves new spawns failing while idle isolates hold the budget. Rejected; admission-triggered
  LRU reclaim is added.
