# Limitations and security assumptions

**Orbit is experimental and not production-ready.** Its isolation properties have not been independently reviewed.
Do not run mutually distrusting tenants on one Orbit process on the strength of this milestone.

## What milestone 1 relies on

| Property | Mechanism | Tested by |
|---|---|---|
| memory isolation between instances | one Wasmtime `Store` per isolate; bounds-checked linear memory | state-isolation tests |
| no ambient authority | only granted `orbit:*` capabilities are linked; WASI and arbitrary imports are rejected at load | denied-capability and unknown-import tests |
| host input validation | every guest `(ptr, len)` is checked against the current memory size with overflow-checked `u64` arithmetic | out-of-bounds / overflow / invalid UTF-8 tests (wasm32 and wasm64) |
| bounded memory | `ResourceLimiter` caps linear memory and tables; admission control caps the sum | memory-growth tests |
| bounded CPU | fuel per invocation; epoch-based wall-clock deadline | runaway-loop tests |
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

* No CPU-time accounting; fuel approximates instruction count, not time.
* Host-side memory used by adapters (KV contents, log buffers) is not charged to the isolate yet; the in-memory KV has
  only per-call size limits.
* The clock has 1 ms resolution by default but no deliberate jitter; timing side channels are not mitigated.
* No process-level sandboxing, seccomp or cgroups yet. The architecture keeps a "one Orbit process per trust domain"
  deployment possible: nothing assumes all tenants share a process.
* Instantiation runs under the invocation fuel budget. Where Wasmtime has no copy-on-write memory image (seen on
  Windows and macOS CI runners), it charges fuel for data-segment initialisation, roughly one unit per byte of the host
  pages touched (4097 for one 4 KiB page). Very small fuel budgets can therefore fail at instantiation on some
  platforms and not on others.
* The epoch ticker bounds deadline precision to one tick (default 10 ms).
* No WASI, no networking, no filesystem, no threads in guests.
* The ABI is version 1 and may still change before a stability declaration.

## Hardening path

1. Fuzz the host ABI (`cargo fuzz` on every capability's argument decoding) and the manifest parser.
2. Charge adapter memory to isolates; per-tenant quotas on KV size.
3. Per-thread CPU time (`CLOCK_THREAD_CPUTIME_ID`) around guest execution slices.
4. Process-per-tenant mode with seccomp-bpf and cgroups v2, using the same embedding API.
5. Independent review before any multi-tenant deployment.
