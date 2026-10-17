# The Orin / Orbit integration boundary

Inspected: [orin-lang/orin-lang](https://github.com/orin-lang/orin-lang) at `6c4104f` (2026-10-17). Orbit does not modify
the compiler; everything under "Proposed" is a request to the compiler project, not a feature.

## 1. What exists in Orin today

| Area | State (verified in the repository) |
|---|---|
| Backends | C (`compiler/backends/c`) and LLVM IR (`compiler/backends/llvm`). **There is no WebAssembly backend.** `docs/roadmap.md` item 8 plans one "at the very end": the LLVM `.ll` goes through `clang --target=wasm64-wasip1` (or `wasm32`) with wasi-libc. |
| Pointer width | `usize` is 64-bit in Orin's IR, so the roadmap names `wasm64` (Memory64) as the first target; `wasm32` needs a target-dependent `usize`. |
| Runtime | `runtime/core` is being ported from C to Orin (ADR-0052, steps R3a..R3f). `runtime/platform` ("L0", `orin_platform.h`, ~590 lines of interface) stays C over libc: allocation, args/env, exit, stdio, files, streams, clocks, threads/monitors, TCP, DNS, policy loading. The roadmap's prerequisite for WebAssembly is that L0 is the only C, so only L0 needs a second implementation. |
| Capabilities (Stage A, ADR-0027) | Accepted and enforced. `main(host: Host)` receives a resource-typed `Host`; scope functions derive **grants** per family (`FsGrant`, `ProcessGrant`, `StdioGrant`, `ClockGrant`; `NetGrant` in `std:net`). Grants can be narrowed (`useUnder`, `useReadOnly`, `useArgsOnly`, `withConnect(host, ports)`) and never widened; they are revoked when the scope returns. Every privileged runtime operation validates the grant at run time. |
| Effects (Stage B, ADR-0021/0037) | `orin effects` infers, per function / module / package / entry point, the transitive set of compiler-owned primitive effects (`Fs.ReadFile`, `Fs.WriteFile`, `Fs.Open`, `Process.Args`, `Process.Spawn`, `Process.Exit`, `Stdio.Read/Write`, `Clock.Now`, `Console.Write`, `Host.Acquire`, plus `Unknown.*`), with evidence and explanation paths. `--format=json` emits `effects/0.1`. `[effects]` in `orin.toml` restricts what a module may perform (E0473). |
| Effect metadata in artifacts | **None.** The effect summary is a separate command output; no compiled artifact carries it. `Console.Write` (`console.log`) is *ambient*: it needs no grant. |
| Concurrency | ADR-0017 stage 0 (`std:task`: structured `scope`/`spawn`/`channel`, no `async`/`await` in 1.0). Tasks use the L0 thread and monitor primitives. |

Consequence: **the Orin compiler cannot produce an artifact Orbit can run today.** Milestone 1 is validated with WAT
test modules (`tests/fixtures`, inline `wat` in the test suites) that exercise exactly the ABI an Orin backend would use.

## 2. The proposed contract

The Orbit host ABI ([host-abi.md](host-abi.md)) is the contract. Its mapping to Orin:

| Orin concept | Orbit ABI v1 | Note |
|---|---|---|
| `console.log` / `Console.Write` | `orbit:log/v1 write` | ambient in Orin, but a *granted* capability in Orbit: a host may deny logging |
| `ClockGrant`, `nowMillis`, `monotonicNow` | `orbit:clock/v1` | |
| (no Orin effect yet) random | `orbit:random/v1` | proposed `Random.Fill` effect |
| `FsGrant` and narrowing | *reserved* `orbit:fs/v1` | see roadmap; WASI preopens are the alternative |
| `NetGrant` | *reserved* `orbit:net/v1` / `orbit:http/v1` | |
| `Host` | the instance's grant set | the host decides; the program's `main(host)` receives what is granted |
| `orin effects` summary | `orbit.manifest` custom section | description only, never authorisation |

### Where the L0 boundary sits

The cleanest cut is **L0**: an Orin `wasm64-orbit` target replaces `runtime/platform/orin_platform.c` by an
implementation whose privileged functions call `orbit:*` imports instead of libc. Everything above L0 (runtime/core in
Orin, std, the program) is unchanged. Allocation, memcpy and formatting stay inside the guest (no host call).

### WASI or `orbit:*`?

The roadmap names `wasm64-wasip1` with wasi-libc. Orbit milestone 1 does **not** link WASI, for three reasons
([ADR-0003](adr/0003-deny-by-default-linking-and-manifests.md)): WASI preview 1 has no notion of Orin's grant narrowing
beyond preopened directories; its ambient `environ_get`, `clock_time_get` and `random_get` would be reachable without a
grant; and the target workloads (HTTP, streams, KV, brokers) are outside WASI preview 1 anyway. A restricted WASI
preview 1 shim (only `fd_write` to the log sink, `proc_exit`, nothing else) is on the roadmap so `clang`-produced
modules can run before an Orbit-specific L0 exists.

## 3. Requests to the Orin compiler project (proposed, not implemented)

1. **A WebAssembly target** (roadmap item 8), `wasm64` first, with an L0 implementation over `orbit:*` imports
   (option A) or over a restricted WASI subset (option B). Orbit supports both memory widths already.
2. **Emit the effect summary into the artifact** as the `orbit.manifest` custom section: `requires` derived from the
   entry point's effects (`Console.Write` -> `log/v1`, `Clock.Now` -> `clock/v1`, ...). An entry point with any
   `Unknown.*` effect should either fail the build for this target or require an explicit allowance, mirroring the
   `[effects]` policy.
3. **Entry-point conventions**: export `memory`, `_initialize` (reactor) instead of `_start` for long-lived services, and
   named exported handler functions; later `orbit_alloc` for host-to-guest payloads.
4. **A `Random.Fill` effect and `RandomGrant`** (there is no randomness primitive in std today).
5. **No threads in the first WebAssembly target**: Orin tasks run sequentially there (as the roadmap already says for
   wasi-threads), until Orbit offers a scheduler-backed task capability.
