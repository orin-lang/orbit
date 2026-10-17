# Orbit host ABI, version 1

Status: **implemented** for `orbit:log/v1`, `orbit:clock/v1`, `orbit:random/v1`, `orbit:kv/v1` (milestone 1) and
`wasi_p1/v1`, the restricted WASI preview 1 shim (milestone 2, section 4a).
Everything marked *reserved* is a design, not a feature.

This document is the contract between a guest module (an Orin program compiled to WebAssembly, or any other module)
and an Orbit host. It is deliberately a **core-WebAssembly** ABI (plain imports and exports with integer parameters),
not a Component Model world; [ADR-0002](adr/0002-core-wasm-abi-before-component-model.md) explains why and how a WIT
version can follow.

## 1. Versioning

* The ABI as a whole has a version (`ORBIT_ABI_VERSION = 1`, crate `orbit-abi`).
* Every capability is versioned on its own. Its import module name is `orbit:<capability>/v<major>`, for example
  `orbit:log/v1`; the one exception is `wasi_p1/v1`, which keeps the literal module name
  `wasi_snapshot_preview1` that standard toolchains emit (section 4a). A host may serve several majors of one capability at once; a guest imports exactly one.
* Inside a major version functions may be **added**, never changed or removed. A guest that imports a function the
  host's version does not have fails to load with `unknown import` (fail closed, never a trap at call time).

## 2. Memory and pointers

* A guest must export its linear memory as `memory` if it imports any function that takes a pointer.
  Both 32-bit memories and 64-bit memories (Memory64) are accepted.
* **Every pointer and every length is an `i64`**, also for a 32-bit memory (the guest zero-extends). Orin's planned
  first WebAssembly target is `wasm64` (`usize` is 64-bit in Orin's IR), so this avoids a second ABI; for wasm32 the
  cost is one `i64.extend_i32_u` per argument.
* The host checks every `(ptr, len)` pair against the memory's **current** size with overflow-checked arithmetic
  before it reads or writes. A negative or out-of-range pair returns `ERR_INVALID` (-1); it never traps the host and
  never reads another instance's memory (each instance has its own store and memory).
* The host never retains a pointer into guest memory after a call returns.

## 3. Status codes

Functions return `i32` status codes, or an `i64` whose negative values are status codes.

| Code | Name | Meaning |
|---:|---|---|
| 0 | `OK` | success |
| -1 | `ERR_INVALID` | bad argument: out-of-bounds buffer, invalid UTF-8, unknown level, ... |
| -2 | `ERR_DENIED` | the operation is linked but refused by the instance's grant (e.g. a KV namespace outside the grant) |
| -3 | `ERR_NOT_FOUND` | no such key / object |
| -4 | `ERR_BAD_HANDLE` | the handle is not live or belongs to another instance |
| -5 | `ERR_TOO_LARGE` | input over the per-call limit, or over a per-capability size limit |
| -6 | `ERR_QUOTA` | a per-instance budget of this capability is exhausted |
| -7 | `ERR_IO` | the host adapter failed |

## 4. Capabilities in version 1

### `orbit:log/v1`

```
write(level: i32, ptr: i64, len: i64) -> i32
```

`level`: 0 trace, 1 debug, 2 info, 3 warn, 4 error. The message must be UTF-8 and at most 16 KiB. Each instance has
a byte budget (default 1 MiB per invocation); beyond it `ERR_QUOTA`. Records go to the host's configured log sink with
the instance id and tenant attached by the host (the guest cannot forge them).

### `orbit:clock/v1`

```
now_unix_nanos() -> i64
monotonic_nanos() -> i64        // since the instance was created
```

The host may coarsen both (default resolution 1 ms) to limit timing side channels.

### `orbit:random/v1`

```
fill(ptr: i64, len: i64) -> i32  // at most 64 KiB per call; cryptographically secure
```

### `orbit:kv/v1`

Key-value storage behind a host adapter (in-memory in milestone 1). Handles are owned by the instance that opened
them.

```
open(name_ptr: i64, name_len: i64) -> i64      // handle > 0, or a negative status (ERR_DENIED if the namespace is not granted)
get(h: i64, key_ptr: i64, key_len: i64, out_ptr: i64, out_cap: i64) -> i64
                                                // the value's full length, or ERR_NOT_FOUND; the value is copied
                                                // only when out_cap >= length (otherwise call again with a larger buffer)
put(h: i64, key_ptr: i64, key_len: i64, val_ptr: i64, val_len: i64) -> i32
delete(h: i64, key_ptr: i64, key_len: i64) -> i32
close(h: i64) -> i32
```

Keys are 1 byte to 1 KiB (an empty key is `ERR_INVALID`), values at most 1 MiB. `delete` of an absent key returns
`OK` (idempotent). At most 64 handles are open per instance (`ERR_QUOTA`). A handle is checked before any buffer, so a bad
handle reports `ERR_BAD_HANDLE` even when a buffer is also invalid. The namespaces an instance may open are part of its grant; the storage of a
namespace is scoped by tenant, so two tenants opening `"cache"` never see each other's keys.

## 4a. `wasi_p1/v1`: restricted WASI preview 1

A compatibility shim so modules built by standard toolchains (clang with wasi-libc, Rust's `wasm32-wasip1`) run
unchanged ([ADR-0007](adr/0007-restricted-wasi-preview1-shim.md)). It is **an ordinary capability**: manifest form
`wasi_p1/v1` (underscore, because capability names are `[a-z0-9_]`), granted with `--grant wasi_p1/v1`, never linked
without a grant. Differences from every `orbit:*` capability:

* the import module is the literal **`wasi_snapshot_preview1`** (`orbit:wasi_p1/v1` is *not* an import module name);
* the signatures are the exact WASI ones: pointers and sizes are **`i32`**, `fd_seek` offset and the `clock_time_get`
  precision are `i64`, functions return a WASI **errno** (`u16` in an `i32`; 0 is success) instead of the ABI status
  codes of section 3, and `proc_exit` returns nothing;
* WASI preview 1 is **wasm32-only**. A module whose exported `memory` is a 64-bit memory and that imports any
  `wasi_snapshot_preview1` function is rejected at load (`LoadError::Memory64Unsupported`). A wasm64 Orin build has to
  use the `orbit:*` capabilities.

Only the functions below exist. **Any other `wasi_snapshot_preview1` import (`path_open`, `fd_pread`, `sock_*`, ...)
fails at load with `unknown import`**, so there is no WASI function that exists but quietly refuses. Pointers are
checked like everywhere else; a bad pointer is `EFAULT` (21), never a trap.

| Function | Behaviour |
|---|---|
| `fd_write(fd, iovs, n, nwritten)` | fd 1 (stdout) -> the host log sink at **info**, fd 2 (stderr) -> **warn**. Output is split into lines per instance and per stream, a record has no trailing newline, an unterminated line is cut at 16 KiB, and whatever is buffered is flushed **when the invocation ends** (return, trap, exit or deadline) and on `fd_close`. A call accepts at most 1 MiB (a longer write is partial) and at most 1024 iovecs (`EINVAL`). Bytes count against the instance's per-invocation log budget (`Limits::max_log_bytes`, also reported in `bytes_logged`); a partial write is returned when the budget runs out, `ENOSPC` (51) when nothing fits. Any other fd, or a closed one: `EBADF` (8). |
| `fd_read(fd, iovs, n, nread)` | stdin is always empty: fd 0 reads 0 bytes (EOF). Other fds: `EBADF`. |
| `fd_close(fd)` | closes fd 0/1/2 (later use is `EBADF`); otherwise `EBADF`. |
| `fd_seek(fd, off, whence, out)` | stdio is a stream: `ESPIPE` (70); other fds `EBADF`. |
| `fd_fdstat_get(fd, buf)`, `fd_filestat_get(fd, buf)`, `fd_fdstat_set_flags(fd, flags)` | fds 0-2 are character devices (filetype 2, size 0, read right for 0, write right for 1 and 2, no path rights); `set_flags` accepts and ignores. Other fds `EBADF`. |
| `fd_prestat_get(fd, buf)`, `fd_prestat_dir_name(fd, p, len)` | always `EBADF`: **there are no preopened directories**. |
| `args_sizes_get`, `args_get` | argv is `["main"]` unless the grant config has `"args": [...]`, which is the *complete* argv (argv[0] included; an empty list gives an empty argv). Non-string entries and entries containing NUL are dropped. |
| `environ_sizes_get`, `environ_get` | empty unless the grant config has `"env": {"K": "V", ...}` (sorted by key; non-string values, empty keys, keys containing `=` and NULs are dropped). The host's own arguments and environment are never visible. |
| `clock_time_get(id, precision, out)`, `clock_res_get(id, out)` | `ENOTCAPABLE` (76) **unless the instance is also granted `clock/v1`**. Then ids 0 (realtime) and 1 (monotonic, since instance creation) use the same time source and the same coarsening (`resolution_ns` of the `clock/v1` grant, default 1 ms) as `orbit:clock/v1`; ids 2 and 3 (CPU time) are `ENOTSUP` (58), others `EINVAL` (28). The requested precision is ignored. |
| `random_get(buf, len)` | `ENOTCAPABLE` **unless the instance is also granted `random/v1`**; then at most 64 KiB per call (`EINVAL` above), OS CSPRNG. Rust's `HashMap` seeds itself with `random_get`, so Rust programs that use one need `random/v1`. |
| `sched_yield()` | success, no effect. |
| `poll_oneoff(...)` | `ENOSYS` (52). |
| `proc_exit(code)` | ends the invocation (section 5). |

**Grant config** (an object, all keys optional): `{"args": ["main", "--flag"], "env": {"MODE": "prod"}}`. On the CLI:
`--grant wasi_p1/v1 --wasi-arg --flag --wasi-env MODE=prod` (`--wasi-arg` values come after `main`).

**`proc_exit`**: the host function unwinds the guest with `GuestExit(code)`. `Isolate::init` / `invoke` then report:

| `code` | Result | Isolate state |
|---|---|---|
| 0 | `Ok` (no return values); `Isolate::exit_code()` is `Some(0)` | `Terminated` (a program that exited cannot be re-entered) |
| non-zero | `Err(ExecError::Exit(code))`; `exit_code()` is `Some(code)` | `Failed` (only `terminate` is possible) |

A trap (including a Rust panic, which aborts on `wasm32-wasip1`) is an `ExecError::Trap`, not an exit.

## 5. Guest exports

| Export | Required | Meaning |
|---|---|---|
| `memory` | if a pointer-taking import is used | the guest's linear memory |
| `_initialize` | no | called once when the instance is initialised (WASI reactor convention) |
| `_start` | no | WASI command entry point; `orbit run` without `--invoke` calls it and exits with the guest's `proc_exit` code (0 if it returns) |
| `orbit_init` | no | called once after `_initialize`; a trap or a non-zero `i32` result fails initialisation |
| any function | — | invocable by name with integer arguments |
| `orbit_alloc(size: i64) -> i64` | *reserved* | for host-to-guest payloads (events, HTTP bodies) in a later version |

## 6. The capability manifest

A module may carry a custom section `orbit.manifest` with UTF-8 JSON:

```json
{"abi": 1, "requires": ["log/v1", "kv/v1"], "producer": "orin 0.1.0"}
```

* The manifest **describes requirements; it never authorises anything.** Authorisation comes only from the host's
  grant (CLI flags or the embedding API).
* When a manifest is present, every capability the module imports must be listed (otherwise load fails with
  `manifest mismatch`); listing a capability that is not imported is allowed (it may be required by a later version).
* Without a manifest the requirements are exactly the module's imports.

## 7. Load-time validation

At load the host:

1. validates and compiles the module (Wasmtime; compiled code is cached per content hash);
2. rejects every import whose module is not a capability the host knows at that major version: an `orbit:` module, or
   `wasi_snapshot_preview1` for the functions listed in section 4a (arbitrary `env` imports and any other WASI
   function are rejected);
3. rejects imports that are not functions (imported memories, tables and globals are not part of ABI v1);
4. checks the import's signature against the capability's definition, and that a wasm32-only capability (WASI) is not
   imported by a module with a 64-bit memory;
5. at instantiation, checks that the instance's grant includes every required capability — a denied capability
   fails instantiation (`capability not granted`); the module never runs.

So a denied capability is not reachable through any supported interface: its functions are not linked.
