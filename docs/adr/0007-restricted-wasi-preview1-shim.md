# ADR-0007: A restricted WASI preview 1 shim as a capability

Status: accepted (2026-10-17)

## Context
Standard toolchains (clang with wasi-libc, Rust's `wasm32-wasip1`, and Orin's planned LLVM-to-wasm path) emit
`wasi_snapshot_preview1` imports. [ADR-0003](0003-deny-by-default-linking-and-manifests.md) made milestone 1 reject
all of them. Programs should run unchanged, but WASI preview 1 is built around ambient authority: files, environment,
wall clock, entropy, process arguments.

## Decision
1. WASI is exposed as one capability, `wasi_p1/v1`, granted and manifested like any other. Capability names are
   `[a-z0-9_]`, so the id uses an underscore. Without the grant the module fails at instantiation (`NotGranted`);
   with no grant the functions are not linked.
2. Its import module is the literal `wasi_snapshot_preview1` (`CapabilitySpec::module` in `orbit-abi`); the
   `orbit:wasi_p1/v1` spelling is not accepted. Signatures are the exact WASI ones (`i32` pointers). The capability is
   marked `wasm32_only`: a module with a 64-bit exported memory that imports it fails at load.
3. **Only implemented functions are in the table.** Everything else (`path_*`, sockets, `fd_pread`, ...) fails at load
   with `unknown import`, in keeping with ADR-0003's "no linked but refusing" rule. Functions that exist but have
   nothing to offer (`poll_oneoff`: `ENOSYS`; `fd_prestat_get`: `EBADF`) are kept because toolchain startup code
   imports them.
4. Authority comes only from the grant:
   * stdout/stderr -> the existing log sink (info/warn), line-buffered per instance, charged to the log budget;
   * stdin empty; no preopens;
   * `args`/`environ` from the grant config only (default argv `["main"]`, empty environment);
   * `clock_*` and `random_get` are `ENOTCAPABLE` unless the instance **also** holds `clock/v1` / `random/v1`. The
     clock functions reuse the clock capability's time source and coarsening instead of a second implementation, so
     one decision (resolution, deterministic test clocks) governs both.
5. `proc_exit` is a host function that returns a `GuestExit(code)` error; Wasmtime unwinds the guest and the isolate
   maps it: code 0 is a clean finish (`Ok`, `exit_code() == Some(0)`, isolate `Terminated`); a non-zero code is
   `ExecError::Exit(code)` with the isolate `Failed`. The CLI exits with the code. A command module (`_start` export) is
   run by `orbit run` without `--invoke`.

## Alternatives considered
* *Wasmtime's `wasmtime-wasi`*: a large surface with filesystem, sockets and environment inheritance, configured by
  denial; its capability model and Orbit's grants would have to be reconciled call by call. A 700-line shim whose table
  is the whole attack surface is easier to audit.
* *Link every WASI function and return `ENOSYS`/`ENOTCAPABLE`*: breaks "denied things are not reachable" and makes
  `orbit inspect` unable to show what a module needs. Not chosen.
* *Make `proc_exit(0)` an error too*: the guest did nothing wrong. Not chosen; but we do not treat the isolate as
  re-enterable after any exit, because its stack was unwound mid-program and C/Rust runtimes keep state in linear
  memory.

## Consequences
* Rust programs using `HashMap`, `SystemTime` or `Instant` need `random/v1` / `clock/v1` granted alongside; otherwise
  std panics and the isolate traps (`Failed`). C programs calling `time()` get an error from libc instead.
* Two extension points were added to the runtime for this: `Capability::end_invocation` (flush buffered output) and
  `InstanceInfo::grants` (see which other capabilities were granted). Both are generic.
* wasm64 Orin output cannot use WASI preview 1 (32-bit pointers by definition); it keeps `orbit:*`.
* Files, when they come, are an `orbit:fs/v1` or a preopen-based extension of this table, decided then; nothing in this
  ADR grants them.
