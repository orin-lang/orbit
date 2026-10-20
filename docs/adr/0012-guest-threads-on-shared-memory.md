# ADR-0012: Guest threads on shared memory

Status: accepted (2026-10-20)

## Context
Orin tasks run truly concurrently when compiled natively. On Orbit the L0 could only run tasks one at a time inside the
module. The goal ([ADR-0010](0010-network-capability-mirrors-the-orin-l0-tcp-layer.md)) is that unchanged Orin code runs
on Orbit, so Orbit needs real threads, in the wasi-threads model, without giving up isolation, deny-by-default grants,
limits and reliable termination.

## Decision
1. **Capability `orbit:thread/v1`** with one function, `spawn(arg: i64) -> i64`, and a guest export
   `orin_thread_start(tid, arg)`. The module imports its memory as a shared `env.memory`; Orbit creates the
   `SharedMemory` per isolate with the memory limit as its maximum. Any other `env.*` import and every unshared or
   64-bit `env.memory` is rejected at load.
2. **A thread is a new instance in its own Store, on its own OS thread**, driven by `Handle::block_on` on the Tokio
   runtime for async host calls. A wait inside wasm blocks the OS thread, so a Tokio worker or a spawn_blocking pool thread
   would be starved or exhausted by waiting guests; the main thread of a threaded isolate therefore also runs on a dedicated
   thread (the `Store` moves to it and back).
3. **Capability state is per isolate**, produced by a new `Capability::thread_state(parent, ..)`: net, fs, kv, process and
   the log budget share one `Arc`; a capability that does not override it gets fresh state, which is documented as
   per-thread.
4. **Termination is isolate-wide.** Phase `RUNNING -> TERMINATING` (first failure wins). Running wasm stops at the epoch
   check (Interrupt), a thread suspended in an async host call has its future dropped, and a thread blocked in
   `memory.atomic.wait*` is woken by the **supervisor notifying every aligned address of the memory** until all threads
   are gone. Alternatives rejected: (a) host `wait` / `notify` imports: reliable and O(1), but they change the contract the
   compiler team agreed (L0 must not use the instructions) and a raw `atomic.wait32` in any other code, such as a C
   library, would still hang termination; (b) requiring finite timeouts: puts the burden on every guest. The sweep costs
   O(memory) per pass and only runs on termination, after a grace of 3 epoch ticks. The supervisor also enforces the
   wall-clock deadline for blocked threads.
5. **A run ends when the main thread returns**; threads still alive are terminated. Fuel is per Store (each thread gets
   the budget). Threaded artifacts are poolable only with fresh-per-acquire.
6. wasm-ld's flag-guarded `__wasm_init_memory` start function is allowed to run in every instance (Wasmtime cannot skip a
   start section; it is a no-op after the first). Modules with active data segments and `spawn` are rejected.
7. **Unsafe.** Reading and writing a `SharedMemory` from the host needs `unsafe` (Wasmtime exposes only
   `&[UnsafeCell<u8>]`). `orbit-runtime` changes `unsafe_code` from `forbid` to `deny` and allows it in exactly one
   module, `shared_mem.rs` (relaxed atomic byte copies), the only `unsafe` in Orbit's own code.

## Consequences
* Threads multiply CPU use: `max_threads` bounds it per isolate, the invocation semaphore does not.
* The memory limit stays exact; per-thread fuel can be `threads x` the budget.
* The wasi shim cannot be combined with threads.
* A malicious guest can hold a lock-protected capability (fs) busy; calls are serialised per isolate by design.
