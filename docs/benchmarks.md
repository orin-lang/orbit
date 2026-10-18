# Benchmarks

Micro-benchmarks for the Orbit runtime (criterion 0.8) and a density example. They measure this implementation on one
machine; they are a regression guard and a way to see where time goes, not a comparison with anything else.

## How to run

```sh
cargo bench -p orbit-runtime                                   # full run (a few minutes)
cargo bench -p orbit-runtime -- --warm-up-time 1 --measurement-time 3   # short run used for the table below
cargo bench -p orbit-runtime -- invoke/                        # one group (criterion name filter)
cargo bench --workspace -- --test                              # smoke test: each benchmark once (this is what CI runs)

cargo run --release -p orbit-runtime --example density -- 1000             # N isolates, on-demand allocator
cargo run --release -p orbit-runtime --example density -- 1000 --pooling   # N isolates, pooling allocator
cargo run --release -p orbit-runtime --example density -- 1000 --pool      # N idle isolates in an InstancePool
```

Benchmark source: `crates/orbit-runtime/benches/runtime.rs`; density: `crates/orbit-runtime/examples/density.rs`.

## What each one measures

| Benchmark | Measures |
|---|---|
| `load/cold_small`, `load/cold_medium_500fn` | `Artifact::load` directly, bypassing the runtime cache: ABI validation plus Cranelift compilation of a one-function module and of a generated 500-function module. Input is already binary (WAT parsed outside the loop). |
| `load/cache_hit_*` | `Runtime::load` of bytes already loaded: SHA-256 of the binary plus a cache lookup. Cost scales with module size because of the hash. |
| `instance/minimal_ondemand`, `instance/minimal_pooling` (and `*_metered`) | `spawn_isolate` + `init` + `terminate` of a module with a 1-page memory and no imports, with Wasmtime's on-demand allocator and with the pooling allocator (64 slots). |
| `instance/log_import_ondemand` | The same lifecycle for a module importing `orbit:log/v1` (linker with one capability, per-instance log state). |
| `invoke/add` | Warm `invoke` of an exported `add(i64, i64)`: the fixed overhead of an invocation (state transitions, fuel/deadline setup, semaphore permit, async call, metrics). |
| `invoke/fib_iter_25` | Iterative Fibonacci(25), a few hundred wasm instructions. |
| `fs_read/chunk_*` (`cargo bench -p orbit-caps`) | A guest reads a 4 MiB file through `orbit:fs/v1` `open`/`read`/`close` in 4 KiB, 64 KiB and 1 MiB calls: host throughput of the file capability including path resolution once per iteration. |
| `invoke/fib_rec_20` | Recursive Fibonacci(20) (about 22k calls), a compute-bound call. |
| `invoke/fib_rec_20_metered`, `invoke/fib_iter_25_metered`, `instance/*_metered` | The same modules on a runtime built with `EngineConfig::fuel_metering = true` (ADR-0008); `Limits::fuel` stays unlimited, so the difference to the unsuffixed rows is the compiled-in accounting. |
| `invoke/host_call_log_32b` | Invocation whose guest makes one `orbit:log/v1` `write` of 32 bytes to a sink that discards it (`NullSink`, defined in the bench): invoke overhead plus one host-call round trip. |
| `pool/spawn_init_invoke_terminate_{ondemand,pooling}` | No pool: spawn + init + one `add` invoke + terminate, the cost of a cold request. |
| `pool/fresh_hit_acquire_invoke_release_*` | `FreshPerAcquire` pool, **prewarmed outside the timed region** (in chunks of 128): `acquire` (hit) + `add` invoke + release, where release terminates the used isolate (so tear-down is included). |
| `pool/fresh_miss_acquire_invoke_release_*` | The same pool with nothing idle: the acquire creates the isolate (pool overhead on top of the cold path). |
| `pool/reuse_acquire_invoke_release_*` | `Reuse` pool with an unlimited `max_uses`: the same isolate serves every request (steady state, state is kept). |
| `pool/heavy_init_*` | A module whose `orbit_init` touches 1 MiB and loops 200k times: cold spawn+init+invoke+terminate versus a prewarmed fresh hit. This is the case pools are for. |
| `pool/spawn_init_invoke_terminate_pooling_{density,latency}` | The pooling allocator with `PoolingConfig::density` (nothing kept resident) versus `::latency` (1 MiB of memory, 64 KiB of table and stack kept resident per free slot). |
| `density` example | Spawns N idle, initialised isolates of a 1-page-memory module (kept alive), reports process `VmRSS` before and after (from `/proc/self/status`; prints `unsupported` elsewhere), the delta per isolate, and p50/p99/max latency of spawn+init. |

Benchmarks run on a single-threaded Tokio runtime with the epoch ticker (10 ms). **Fuel metering is off** (Orbit's
default, [ADR-0008](adr/0008-fuel-metering-is-opt-in.md)) except in the rows suffixed `_metered`. No fuel or timeout
limit is set.

## Results

Machine: 4 logical CPUs (`nproc` = 4), Intel(R) Xeon(R) Processor @ 2.10GHz (virtualised, as reported by
`/proc/cpuinfo`), Linux 6.18, rustc 1.97.0, wasmtime 49, `--release`. Criterion short run
(`--warm-up-time 1 --measurement-time 3`); values are the median estimate with the criterion interval in brackets.

| Benchmark | Median [interval] |
|---|---|
| load / cold, small module | 740 µs [715, 768] µs |
| load / cache hit, small module | 139 ns [137, 142] ns |
| load / cold, 500-function module | 192.5 ms [187.6, 197.8] ms |
| load / cache hit, 500-function module | 19.2 µs [19.0, 19.4] µs |
| instance / minimal, on-demand | 6.76 µs [6.66, 6.87] µs |
| instance / minimal, pooling | 2.78 µs [2.71, 2.85] µs |
| instance / module importing log, on-demand | 8.96 µs [8.80, 9.17] µs |
| invoke / `add` | 1.03 µs [1.00, 1.06] µs |
| invoke / `fib_iter_25` | 1.07 µs [1.03, 1.11] µs |
| invoke / `fib_rec_20` | 114 µs [112, 115] µs |
| invoke / host call (log 32 B) | 1.35 µs [1.29, 1.42] µs |

The rows above were taken when fuel metering was always on (before ADR-0008), so they are **metered** numbers. The
`fib_rec_20` row and several others were re-measured in both modes, see below.

### Fuel metering on and off (ADR-0008)

Second short run on the same machine (`cargo bench -p orbit-runtime --bench runtime -- "invoke/|instance/"
--warm-up-time 1 --measurement-time 3`, and a 5 s run of `fib_rec_20` alone):

| Benchmark | Unmetered (default) | Metered |
|---|---|---|
| invoke / `fib_rec_20` (3 s run) | 63.0 µs [60.5, 65.8] µs | 111.7 µs [110.8, 112.8] µs |
| invoke / `fib_rec_20` (5 s run) | 62.4 µs [60.4, 64.7] µs | 114.6 µs [113.5, 115.8] µs |
| invoke / `fib_iter_25` | 1.06 µs [1.02, 1.10] µs | 1.05 µs [1.03, 1.07] µs |
| invoke / `add` | 1.01 µs [1.00, 1.04] µs | (not run metered) |
| instance / minimal, on-demand | 8.71 µs [8.37, 9.07] µs | 8.94 µs [8.51, 9.40] µs |
| instance / minimal, pooling | 2.85 µs [2.76, 2.97] µs | 2.94 µs [2.84, 3.05] µs |

On the compute-bound recursive call metering costs about 1.8x here (62-63 µs without, 112-115 µs with). That is the
same order as the 1.2x to 1.8x measured by the Orin compiler's WebAssembly work on its programs (see ADR-0008), whose
`fib` is also call-heavy; loop-heavy and allocation-heavy code pays less. Invocation overhead (`fib_iter_25`, `add`) and
instance creation do not change measurably: their cost is host-side setup, not compiled guest code. (The on-demand
instance row differs from the first table because it comes from another run on a noisy VM.)
Density, 1000 isolates, 1 page (64 KiB) of declared memory each, one run per row:

| Allocator | RSS delta | Per isolate | Spawn+init p50 | p99 | max |
|---|---|---|---|---|---|
| on-demand | 9.4 MB | about 9.4 KB | 13.4 µs | 45.5 µs | 239 µs |
| pooling (1000 slots, 1 MiB max) | 9.4 MB | about 9.4 KB | 8.9 µs | 41.0 µs | 433 µs |

### Instance pools and allocator tuning (milestone 3)

Same machine, short run (`cargo bench -p orbit-runtime --bench runtime -- pool/ --warm-up-time 1 --measurement-time 3`),
taken while other builds were running on the same 4 vCPUs, so absolute values are inflated compared with the first
table (the on-demand spawn row here is about 21 µs where the first table has about 7 µs). Compare rows within this
table only. Median [interval].

| Per request | on-demand allocator | pooling allocator (512 slots) |
|---|---|---|
| spawn + init + invoke + terminate (no pool) | 21.9 µs [20.7, 23.2] | 4.60 µs [4.42, 4.79] |
| `FreshPerAcquire`, prewarmed hit: acquire + invoke + release | 18.2 µs [17.4, 19.1] | 3.52 µs [3.35, 3.73] |
| `FreshPerAcquire`, miss (acquire creates) | 21.8 µs [20.9, 22.7] | 4.79 µs [4.64, 4.96] |
| `Reuse`, steady state | 1.56 µs [1.48, 1.65] | 1.31 µs [1.27, 1.35] |

| Heavy-init module (1 MiB touched, 200k-iteration `orbit_init`), on-demand | Median [interval] |
|---|---|
| spawn + init + invoke + terminate | 1.04 ms [0.97, 1.13] |
| `FreshPerAcquire`, prewarmed hit: acquire + invoke + release | 265 µs [225, 302] |

| Pooling allocator, spawn + init + invoke + terminate, 1-page module | Median [interval] |
|---|---|
| `PoolingConfig::density` (nothing kept resident) | 4.45 µs [4.28, 4.65] |
| `PoolingConfig::latency` (1 MiB memory, 64 KiB table/stack kept resident) | 5.78 µs [5.62, 5.95] |

Density example with `--pool` (1000 idle, prewarmed isolates, 1 page of declared memory, `Limits::max_memory_bytes` =
1 MiB, one run each):

| Allocator | Prewarm | RSS per idle pooled isolate | Admission reserved per isolate | Acquire (hit) p50 / p99 | `reclaim_idle` |
|---|---|---|---|---|---|
| on-demand | 16.4 ms for 1000 | about 9.8 KB | 1 MiB (1000 MiB total) | 383 ns / 792 ns | reserved 1000 MiB -> 0; RSS 22.9 MB -> 18.8 MB |
| pooling | 19.6 ms for 1000 | about 9.7 KB | 1 MiB | 154 ns / 6.4 µs | reserved 1000 MiB -> 0; RSS unchanged (slots stay resident) |

What these say, and do not say:

* A prewarmed hit takes the instantiation and initialisation off the request path. With a trivial module that is
  worth only a few microseconds, because instantiation itself is cheap; the **tear-down of the used isolate remains in
  the request path** (release terminates synchronously), which is why the fresh hit is only about 15% below the cold
  path here. With a module that has real start-up work the saving is large (1.04 ms -> 265 µs; most of the remaining
  265 µs is tearing down a store with 1 MiB of touched memory). Moving termination off the request path is possible
  future work.
* `Reuse` is about 12x (on-demand) and 2.7x (pooling) cheaper than a fresh hit for the trivial module, and it is the only mode where guest
  state survives. Its number is the fixed overhead of acquire + invoke + release, not a statement about real programs.
* The density example's "acquire" time stops when `acquire` returns; the release (termination) is not included.
* Idle pooled isolates cost RSS (about 9.8 KB each here) and, much more importantly for admission, the full memory
  *limit* in the budget (1 MiB each): 1000 idle isolates reserve 1000 MiB. `reclaim_idle` returned all of it. With the
  pooling allocator RSS did not drop after reclaim in this run (slot memory is retained by the allocator, and no
  keep-resident bytes were configured), so do not read admission-budget release as an RSS release.
* The `latency` preset was **slower** than `density` on this 1-page module (5.8 vs 4.5 µs, with overlapping noise
  from a shared VM): keeping memory resident helps modules that touch much more memory per request, which this one
  does not. Measure with your own module before choosing a preset; no claim is made that either is better in general.
* The "concurrent 500 acquires" scenario is a correctness test (`tests/pool.rs`), not a benchmark.

### Network echo through a guest (`orbit:net/v1`)

`cargo run --release -p orbit-caps --example net_load -- <isolates> <clients per isolate> <seconds>`. Each isolate runs a
WAT server (accept, recv, send, close, repeat; one connection at a time) on its own loopback listener; each client
connects, sends 64 bytes, reads them back and closes (connect included in the latency). Same machine as above (4
logical CPUs, virtualised), 5 s runs, measured once:

| Isolates x clients | Requests/s | p50 | p99 |
|---|---|---|---|
| 1 x 8 | 20 700 | 312 µs | 1.5 ms |
| 4 x 8 | 19 900 | 687 µs | 16.6 ms |
| 8 x 8 | 19 900 | 2.2 ms | 18.2 ms |

Caveats: client and server share the 4 CPUs and one process, so this measures the whole loopback path (TCP connect and
close per request dominate), not the host calls alone; the plateau at about 20 000 req/s is the machine, not a limit of
the runtime. Each guest is single-threaded, so more isolates only add queueing here. One run, no confidence interval.

## Reading the numbers, and caveats

* This is a shared cloud VM with other tenants: results are noisy, the criterion intervals only capture noise during
  the run, and numbers will differ on other hardware. Treat differences below roughly 20% as noise. The density rows
  are single runs.
* The compute cost is tiny next to the fixed costs: an empty-ish invocation is about 1 µs, and the host-call round trip
  adds roughly 0.1 to 0.3 µs on top of it in this setup (two runs gave 1.10 and 1.35 µs, so the exact figure is within noise).
* Cold compilation dominates: loading a 500-function module costs about 190 ms, versus about 19 µs when cached, which
  is why the artifact cache and sharing compiled code between isolates matter. Cold time is single-threaded Cranelift
  on a synthetic module and says nothing about real-world modules.
* Density: RSS counts resident pages only. A module whose memory is mostly untouched costs little resident memory, and
  the pooling allocator reserves its slots as virtual address space, which RSS does not show. The delta includes all
  host-side per-isolate state (store, fiber stack bookkeeping, handle tables) and allocator behaviour, but not
  virtual memory. The idle isolates here never ran guest code beyond init, so real applications will use more.
  The two allocators showing the same RSS delta is plausible for this workload but only one run each was taken.
* The pooling instance benchmark reuses slots across iterations, so it measures the warm-slot path.
* The invocation rows come from a second run (after switching the shared isolate to a `tokio::sync::Mutex`); the
  other rows are from the first run.
* Invocation benchmarks reuse one isolate and pass `max_log_bytes` large enough that the log budget is never hit.
* The metered-versus-unmetered comparison is one VM, two short runs, one micro-benchmark with a call-dominated profile
  (22k calls, no memory traffic). It shows the direction and rough size of the effect, not a general factor; the Orin
  figures in ADR-0008 are the better indication for real programs.
* No comparison with other runtimes, container or process-based approaches, or native code was made, and none is
  implied. These numbers should not be used to argue that Orbit is faster or smaller than anything else.

## Orin programs (compiled Orin code in Orbit)

A real Orin program, `tests/guests/orin/compute` (`fib(35)`, a sieve of Eratosthenes over 5M flags, Collatz steps for
1..300000, 1000 small strings), built with `orin build --release` (Orin commit in
`crates/orbit-cli/tests/fixtures/orin/VERSION`) natively and for `wasm32-orbit` / `wasm64-orbit`, run with the release
`orbit` binary (`orbit run compute_NN.wasm --grant log/v1 --max-memory 268435456`, **unmetered**). Wall time of the whole
process, 6 runs each, `python3 time.perf_counter`, same VM as above (4 vCPU Xeon 2.10GHz, shared, noisy; other jobs ran on it):

| Run | Median | Range (6 runs) |
|---|---|---|
| native, Orin LLVM backend (release default) | 0.247 s | 0.218 to 0.315 s |
| native, Orin C backend (`--backend=c`, clang -O2) | 0.266 s | 0.223 to 0.293 s |
| Orbit, `wasm32-orbit` | 0.536 s | 0.437 to 0.589 s |
| Orbit, `wasm64-orbit` | 0.568 s | 0.475 to 0.660 s |
| Orbit, `wasm32-orbit`, `--fuel 100000000000` (metered) | 0.636 s | 0.540 to 0.650 s |
| `hello` (3 log lines), wasm32 / wasm64 | 0.051 s / 0.081 s | 0.037 to 0.110 s |

Caveats: the Orbit rows include process start, module validation and Cranelift compilation of a roughly 110 KB module
(the `hello` row, almost all of it fixed cost, is 40 to 80 ms), so the compute-only gap is smaller than the table
suggests: about 2x native overall, of which roughly 50 to 80 ms is load. The wasm32 versus wasm64 difference (about 6%) is
within the noise of this machine, so these runs do not confirm or refute the Orin team's per-program figures in
[orin-integration.md](orin-integration.md) section 2a. Orin `int` is 8 bytes, so this program needs more than the default
16 MiB memory limit (the sieve alone is 40 MB): with the default limit it traps with `out of memory` (a test pins this).
One machine, one program: not a comparison with other runtimes.
