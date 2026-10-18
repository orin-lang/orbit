# Limitations and security assumptions

**Orbit is experimental and not production-ready.** Its isolation properties have not been independently reviewed.
Do not run mutually distrusting tenants on one Orbit process on the strength of this milestone.

## What milestone 1 relies on

| Property | Mechanism | Tested by |
|---|---|---|
| memory isolation between instances | one Wasmtime `Store` per isolate; bounds-checked linear memory | state-isolation tests |
| no ambient authority | only granted capabilities are linked (`orbit:*` and the `wasi_p1/v1` shim); arbitrary imports and every WASI function the shim does not implement (all path, socket and file functions) are rejected at load | denied-capability and unknown-import tests |
| WASI has no hidden authority | stdout/stderr go to the log sink under the log budget; stdin is empty; no preopens; argv/env only from the grant; clock and random answer `ENOTCAPABLE` unless `clock/v1` / `random/v1` are also granted | `crates/orbit-caps/tests/wasi.rs`, `wasi_toolchain.rs` |
| host input validation | every guest `(ptr, len)` is checked against the current memory size with overflow-checked `u64` arithmetic | out-of-bounds / overflow / invalid UTF-8 tests (wasm32 and wasm64) |
| bounded memory | `ResourceLimiter` caps linear memory and tables; admission control caps the sum | memory-growth tests |
| bounded CPU | wall-clock deadline (`Limits::timeout`, epoch based); with `EngineConfig::fuel_metering` also fuel per invocation. **Without metering and without a timeout nothing bounds a runaway guest**: always set a timeout in multi-tenant use ([ADR-0008](adr/0008-fuel-metering-is-opt-in.md)) | runaway-loop tests (`limits.rs`, `metering.rs`) |
| handle ownership | per-instance handle tables with generations | KV handle tests |
| tenant separation in shared adapters | adapters are keyed by tenant chosen by the host, never by the guest | KV tenant tests |
| cleanup | a trapped isolate is `Failed`, refuses work, and its store is dropped on terminate | cleanup tests |

## Assumptions (trusted computing base)

* **Wasmtime and Cranelift are correct.** A miscompilation or a sandbox escape in the engine breaks every guarantee
  above. Mitigation path: track Wasmtime security advisories, keep the version current, fuzz the host ABI.
* **The host capability code is correct.** It is ordinary Rust running with the process's full authority. It is kept
  small, uses no `unsafe` (`#![forbid(unsafe_code)]` in every Orbit crate), and validates every guest value.
* **The process boundary is the real security boundary.** WebAssembly isolation does not protect against
  side channels (Spectre-class, timing, cache), against bugs in the engine, or against resource exhaustion of
  shared host resources that Orbit does not yet meter.
* **Compiler checks are not a security property.** An Orin program that passed `orin check`, `orin verify` and the
  `[effects]` policy is still treated as untrusted code: its manifest is descriptive and every host call is checked.

## Known limitations

* **Network (`orbit:net/v1`).** The grant scopes (`net:listen=...`, `net:connect=...`) are matched against the *text* the
  guest passes, as in Orin's L0. A name covered by `*.example.com` that resolves to a loopback or private address is
  reachable, and a DNS answer can change between the scope check and the connect: do not use host patterns as a
  firewall; for hard guarantees run Orbit in a network namespace or behind an egress proxy. `dns_resolve` cannot be
  cancelled once started (30 s bound). Quotas bound open sockets (default 64) and bytes per call (1 MiB) per isolate,
  not total bandwidth, and nothing limits the number of connections a *listening* guest accepts, so a guest granted
  `listen` is a server that must do its own admission. A listener on `0.0.0.0` is reachable from the network: grant the
  narrowest address. Waits end at the isolate's wall-clock deadline, but a server without a deadline relies on fuel and
  memory limits for CPU and on the process boundary for everything else. Tests: `crates/orbit-caps/tests/net.rs`.
* No CPU-time accounting; fuel (opt-in) approximates instruction count, not time. Without fuel metering the CPU bound is
  the epoch deadline: wall-clock, tick-granular (10 ms by default) and not deterministic. The `orbit run` CLI sets no
  timeout by default, so a runaway guest runs until killed unless `--timeout-ms` or `--fuel` is given.
* Host-side memory used by adapters (KV contents, log buffers) is not charged to the isolate yet; the in-memory KV has
  only per-call size limits.
* The clock has 1 ms resolution by default but no deliberate jitter; timing side channels are not mitigated.
* No process-level sandboxing, seccomp or cgroups yet. The architecture keeps a "one Orbit process per trust domain"
  deployment possible: nothing assumes all tenants share a process.
* On a metered engine, instantiation runs under the invocation fuel budget. Where Wasmtime has no copy-on-write memory image (seen on
  Windows and macOS CI runners), it charges fuel for data-segment initialisation, roughly one unit per byte of the host
  pages touched (4097 for one 4 KiB page). Very small fuel budgets can therefore fail at instantiation on some
  platforms and not on others.
* The epoch ticker bounds deadline precision to one tick (default 10 ms).
* WASI is a restricted preview 1 subset ([host-abi.md](host-abi.md) section 4a): no filesystem (no preopens, no
  `path_*`), no sockets, no `poll_oneoff`, no threads, no networking in guests. It is wasm32-only. Guest stdout and
  stderr are untrusted text delivered to the host's log sink: sinks must treat it as data (a line can contain
  anything, including terminal escape sequences).
* `proc_exit(0)` terminates the isolate, so a WASI command cannot be invoked twice; run a command module once, or
  build a reactor (`_initialize`) for repeated calls.
* Instance pools ([ADR-0009](adr/0009-instance-pools-fresh-by-default.md)) are fresh-per-acquire by default. With
  `ReusePolicy::Reuse` an isolate keeps its memory, globals, tables and capability handles across requests of the
  same tenant; do not enable it when requests of one tenant must be isolated from each other. A pool is bound to one
  tenant and one grant set, so state never crosses tenants.
* The ABI is version 1 and may still change before a stability declaration.

## Hardening path

1. Fuzz the host ABI (`cargo fuzz` on every capability's argument decoding) and the manifest parser.
2. Charge adapter memory to isolates; per-tenant quotas on KV size.
3. Per-thread CPU time (`CLOCK_THREAD_CPUTIME_ID`) around guest execution slices.
4. Process-per-tenant mode with seccomp-bpf and cgroups v2, using the same embedding API.
5. Independent review before any multi-tenant deployment.
