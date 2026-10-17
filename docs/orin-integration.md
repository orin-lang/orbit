# The Orin / Orbit integration boundary

Inspected: [orin-lang/orin-lang](https://github.com/orin-lang/orin-lang) at `6c4104f` (2026-10-17). Orbit does not modify
the compiler; the table in section 1 is the state at that commit. Section 2a records the entry-point contract agreed
with the Orin compiler team afterwards (per the Orin WebAssembly work, 2026-10-17); the requests in section 3 are
requests to the compiler project, not Orbit features.

## 1. What exists in Orin today

| Area | State (verified in the repository) |
|---|---|
| Backends | C (`compiler/backends/c`) and LLVM IR (`compiler/backends/llvm`). **There is no WebAssembly backend at that commit** (a wasm64 target is now in progress on the Orin side, see section 2a). `docs/roadmap.md` item 8 plans one "at the very end": the LLVM `.ll` goes through `clang --target=wasm64-wasip1` (or `wasm32`) with wasi-libc. |
| Pointer width | `usize` is 64-bit in Orin's IR, so the roadmap names `wasm64` (Memory64) as the first target; `wasm32` needs a target-dependent `usize`. |
| Runtime | `runtime/core` is being ported from C to Orin (ADR-0052, steps R3a..R3f). `runtime/platform` ("L0", `orin_platform.h`, ~590 lines of interface) stays C over libc: allocation, args/env, exit, stdio, files, streams, clocks, threads/monitors, TCP, DNS, policy loading. The roadmap's prerequisite for WebAssembly is that L0 is the only C, so only L0 needs a second implementation. |
| Capabilities (Stage A, ADR-0027) | Accepted and enforced. `main(host: Host)` receives a resource-typed `Host`; scope functions derive **grants** per family (`FsGrant`, `ProcessGrant`, `StdioGrant`, `ClockGrant`; `NetGrant` in `std:net`). Grants can be narrowed (`useUnder`, `useReadOnly`, `useArgsOnly`, `withConnect(host, ports)`) and never widened; they are revoked when the scope returns. Every privileged runtime operation validates the grant at run time. |
| Effects (Stage B, ADR-0021/0037) | `orin effects` infers, per function / module / package / entry point, the transitive set of compiler-owned primitive effects (`Fs.ReadFile`, `Fs.WriteFile`, `Fs.Open`, `Process.Args`, `Process.Spawn`, `Process.Exit`, `Stdio.Read/Write`, `Clock.Now`, `Console.Write`, `Host.Acquire`, plus `Unknown.*`), with evidence and explanation paths. `--format=json` emits `effects/0.1`. `[effects]` in `orin.toml` restricts what a module may perform (E0473). |
| Effect metadata in artifacts | **None at that commit** (emitting `orbit.manifest` is in progress on the Orin side, see section 2a). The effect summary is a separate command output; no compiled artifact carries it. `Console.Write` (`console.log`) is *ambient*: it needs no grant. |
| Concurrency | ADR-0017 stage 0 (`std:task`: structured `scope`/`spawn`/`channel`, no `async`/`await` in 1.0). Tasks use the L0 thread and monitor primitives. |

Consequence at that commit: **the Orin compiler could not produce an artifact Orbit can run.** Orbit is validated with WAT
test modules (`tests/fixtures`, inline `wat` in the test suites) that exercise exactly the ABI an Orin backend uses,
including the entry-point contract of section 2a (`crates/orbit-cli/tests/fixtures/orin_*.wat`).

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

The roadmap names `wasm64-wasip1` with wasi-libc. Orbit serves a **restricted** WASI preview 1 (`wasi_p1/v1`,
[ADR-0007](adr/0007-restricted-wasi-preview1-shim.md), [host-abi.md](host-abi.md) §4a) for **wasm32 modules only**:
stdout/stderr go to the log sink, there are no preopened directories, argv and environment come only from the grant,
and clocks and randomness work only when `clock/v1` / `random/v1` are granted as well. An Orin `wasm32-wasip1` build
therefore runs unchanged. WASI preview 1 is not defined for 64-bit memories, so **wasm64 needs the `orbit:*` L0
(option A)**; that is the path the Orin WebAssembly work takes (wasm64, own L0, importing only `orbit:*`: `orbit:log/v1`
and `orbit:clock/v1`).

## 2a. The Orin entry-point contract (agreed)

Agreed with the Orin compiler team, per the Orin WebAssembly work, 2026-10-17: a wasm64 build with Orin's own L0
(option A above).

| Item | Contract |
|---|---|
| Target | wasm64 (Memory64), Orin's own L0 over `orbit:*` imports (option A) |
| Imports | only `orbit:log/v1` and `orbit:clock/v1` |
| WASI | none (no `wasi_snapshot_preview1` imports) |
| Export `memory` | the linear memory (64-bit) |
| Export `orin_main() -> i32` | the program entry point; the result is the exit status |
| Export `orin_exit_status() -> i32` | after `orin_main` trapped: the exit status the program set, or `-1` for a genuine trap |
| Custom section `orbit.manifest` | generated from Orin's effect analysis. With unresolved calls it conservatively also lists `clock/v1` |

How Orbit honours it ([host-abi.md](host-abi.md) section 5):

* `orbit run module.wasm` on a module that exports `orin_main` (and no `_start`, no `--invoke`) calls `orin_main` and
  exits with its result (a non-zero status that wraps to 0 modulo 256 becomes 1, as for `proc_exit`).
* If `orin_main` traps, the CLI calls `orin_exit_status` once (library: `Isolate::post_mortem_i32`) under a small fresh
  budget. A status other than `-1` is the exit status, and the trap message is still printed to stderr. `-1`, a missing
  export or a failing call mean a genuine trap: the original trap is reported and the exit status is 1.
* The post-mortem call is allowed only after a *trap*, not after fuel exhaustion, a deadline, a memory-limit trap or
  `proc_exit`, and the isolate stays `Failed`.
* Fuel metering is off by default ([ADR-0008](adr/0008-fuel-metering-is-opt-in.md)). Orin programs that need a CPU bound
  get the wall-clock deadline (`--timeout-ms`); fuel needs `--fuel`.

Measured cost of Memory64 (reported by the Orin compiler team, C reference programs, wasm32 versus wasm64):

| Program | wasm64 versus wasm32 |
|---|---|
| fib | +17 % |
| bintrees | +30 % |
| nbody | +39 % |
| sieve | -13 % |

These come from a **C reference implementation**, not from Orin code, and Orbit did not reproduce them. A wasm32 Orin
target would avoid the cost (Wasmtime bounds checks on 64-bit memories are explicit rather than guard-page based) but
needs a target-dependent `usize` in Orin. `wasi_p1/v1` is wasm32-only, so a wasm32 build could use WASI instead of its
own L0 (option B).

## 3. Requests to the Orin compiler project

Status per the Orin WebAssembly work, 2026-10-17: requests 1, 2 and 3 are **in progress on the Orin side**; 4 and 5
are open.

1. **A WebAssembly target** *(in progress on the Orin side)* (roadmap item 8), `wasm64` first, with an L0 implementation over `orbit:*` imports
   (option A) or over a restricted WASI subset (option B). Orbit supports both memory widths already.
2. **Emit the effect summary into the artifact** *(in progress on the Orin side)* as the `orbit.manifest` custom section: `requires` derived from the
   entry point's effects (`Console.Write` -> `log/v1`, `Clock.Now` -> `clock/v1`, ...). An entry point with any
   `Unknown.*` effect should either fail the build for this target or require an explicit allowance, mirroring the
   `[effects]` policy.
3. **Entry-point conventions** *(in progress on the Orin side)*: export `memory`, `orin_main` and `orin_exit_status`
   (section 2a); for long-lived services `_initialize` (reactor) instead of `_start` and named exported handler
   functions; later `orbit_alloc` for host-to-guest payloads.
4. **A `Random.Fill` effect and `RandomGrant`** (there is no randomness primitive in std today).
5. **No threads in the first WebAssembly target**: Orin tasks run sequentially there (as the roadmap already says for
   wasi-threads), until Orbit offers a scheduler-backed task capability.
