# Orbit host ABI, version 1

Status: **implemented** for `orbit:log/v1`, `orbit:clock/v1`, `orbit:random/v1`, `orbit:kv/v1` (milestone 1),
`wasi_p1/v1`, the restricted WASI preview 1 shim (milestone 2, section 4a), then `orbit:fs/v1`, `orbit:process/v1` and
`orbit:net/v1` (milestone 3, sections 4b to 4d).
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
| -8 | `ERR_EOF` | orderly end of a stream (`orbit:net/v1` `recv`) |
| -9 | `ERR_TIMEOUT` | a waiting call hit its timeout (net, `process.stdin_read`) |
| -10 | `ERR_RESET` | connection reset, aborted, or broken pipe |
| -11 | `ERR_CANCELLED` | a waiting call was cancelled through its wake source |
| -12 | `ERR_EXISTS` | the object already exists |
| -13 | `ERR_NOT_DIR` | a path component is not a directory |
| -14 | `ERR_IS_DIR` | the path names a directory where a file is needed |
| -15 | `ERR_REFUSED` | the peer refused the connection (net `tcp_connect`) |
| -16 | `ERR_ADDR_IN_USE` | the address is already in use (net `tcp_listen`) |

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

## 4b. `orbit:fs/v1`: files under virtual mounts

Designed so the Orin L0 platform layer's file functions (`orin_platform_file_*`, `dir_list`, `stream_*`, `path_*`) can be
implemented faithfully, and so unchanged `std:fs` code runs on Orbit ([ADR-0011](adr/0011-fs-and-process-capabilities-mirror-the-orin-l0-layer.md)).
Functions returning `i64` return a value `>= 0` or a negative status; handles are `> 0`. **Whenever an output buffer is
too small the call returns `ERR_TOO_LARGE` and writes nothing.**

```
open(path_ptr: i64, path_len: i64, mode: i32) -> i64     // handle; mode 0 read, 1 write (create/truncate), 2 append (create)
read(h: i64, out_ptr: i64, out_cap: i64) -> i64          // bytes read, 0 = end of file; may be short
write(h: i64, ptr: i64, len: i64) -> i64                 // bytes written; may be short (loop)
flush(h: i64) -> i32
close(h: i64) -> i32                                     // also closes directory handles
stat(path_ptr: i64, path_len: i64) -> i32                // 0 none, 1 file (anything not a directory), 2 directory
size(path_ptr: i64, path_len: i64) -> i64                // size of a file in bytes
dir_open(path_ptr: i64, path_len: i64) -> i64            // handle
dir_next(h: i64, out_ptr: i64, out_cap: i64) -> i64      // name length | 0x4000_0000_0000_0000 if a directory; 0 = end
dir_close(h: i64) -> i32
realpath(path_ptr: i64, path_len: i64, out_ptr: i64, out_cap: i64) -> i64   // length of the virtual canonical path
cwd(out_ptr: i64, out_cap: i64) -> i64                   // length of the virtual working directory (absolute)
```

**Authority is the mounts in the grant.** A mount binds a virtual absolute path to a host directory, read-only or
read-write. CLI: `--grant fs:/data=./hostdir:ro` (`:rw` for write; read-only if the suffix is omitted; repeatable,
several mounts allowed; mounts may not overlap or nest). Embedders use `FsGrant` (`orbit-caps`), whose JSON is
`{"mounts":[{"guest":"/data","host":"/abs/dir","writable":false}],"cwd":"/","max_handles":64,"max_write_bytes":null}`.
A malformed config grants nothing.

**Path resolution.** A guest path is bytes that must be UTF-8 without NUL (`ERR_INVALID`), non-empty (`ERR_NOT_FOUND`)
and at most 4096 bytes (`ERR_TOO_LARGE`). A relative path resolves against the virtual cwd (`/`, or `--cwd`). The
result is normalised lexically (`.` and empty components dropped, `..` removes the previous component and is dropped at
the root), like the L0's `path_normalize`. The normalised path must lie in a mount, otherwise **`ERR_DENIED`**; so
`/data/../etc/passwd` and any host absolute path are denied. Inside a mount the remainder is resolved relative to the
mount's directory handle by the OS-level confinement of `cap-std`: `..` that leaves the mount (for example through a
symlink), absolute symlinks and symlinks that point outside are `ERR_DENIED`; symlinks that stay inside are followed.
A write (`open` with mode 1 or 2) on a read-only mount is `ERR_DENIED` **before any OS call**. A proper ancestor of a
mount (`/` when mounts are `/data` and `/work`) is a read-only virtual directory: `stat` says 2, `dir_open` lists the
mount names, `open` is `ERR_IS_DIR`.

**Semantics.**
* `open`: a directory is `ERR_IS_DIR`; a path component that is a file is `ERR_NOT_DIR`; a missing file (mode 0) or a
  missing parent (modes 1, 2) is `ERR_NOT_FOUND`; a non-regular file (FIFO, device) is `ERR_DENIED` and never opened.
  `read` on a write handle and `write` on a read handle are `ERR_INVALID`. The host does not buffer: `write` hands bytes
  to the OS, `flush` is a no-op on the host side (the L0 buffers).
* `read` returns at most 1 MiB per call (a larger `out_cap` is clamped, so a short read is not end of file; only 0 is).
  `write` accepts at most 1 MiB per call and returns the count written; the caller loops. A bad output range is
  `ERR_INVALID` **before** any byte is consumed.
* `stat`: a missing path is `0` (not an error); `ERR_DENIED` outside the mounts; the size comes from `size` (a missing
  file is `ERR_NOT_FOUND`, a directory `ERR_IS_DIR`). A whole-file read is `size` + `open` + `read` loop + `close`.
* `dir_next` enumerates a directory opened by `dir_open` in OS order, without `.` and `..`. The directory flag is true
  only for real directories (a symlink to a directory is **not** flagged). A name that does not fit in `out_cap` gives
  `ERR_TOO_LARGE` and is **not consumed**: call again with a bigger buffer. Names that are not valid UTF-8 are skipped.
  `0` means the end; an I/O error in the middle is a negative status and the caller discards the partial listing.
* `realpath`: the longest existing prefix with symlinks resolved, then the non-existing rest unchanged; the result is a
  **virtual** path (never a host path). `ERR_NOT_FOUND` if the first missing component is a dangling symlink, `ERR_DENIED`
  for an escaping link.
* Handles (files and directories) live in the per-instance `HandleTable`; a stale, forged or foreign handle is
  `ERR_BAD_HANDLE`; a handle of the wrong kind (`dir_next` on a file) is `ERR_INVALID`.

**Quotas.** Open handles per instance (default 64, `--fs-max-handles`, `ERR_QUOTA` on `open`/`dir_open`); at most
1 MiB per `read`/`write` call; optional total bytes written over the instance's life (`--fs-max-write-bytes`; a
`write` that would pass it is `ERR_QUOTA` and writes nothing).

**Errors.** Host failures become status codes only: not found, denied (including OS permission errors), is-a-directory,
not-a-directory, name too long (`ERR_TOO_LARGE`) have their own codes; everything else (disk full, I/O error, link loop,
too many host files) is `ERR_IO`. Host paths and OS error text never reach the guest.

## 4c. `orbit:process/v1`: arguments, environment, standard input

```
arg_count() -> i32
arg(i: i32, out_ptr: i64, out_cap: i64) -> i64                       // length; ERR_INVALID if i is out of range
env_get(name_ptr: i64, name_len: i64, out_ptr: i64, out_cap: i64) -> i64   // length; ERR_NOT_FOUND if unset
stdin_read(out_ptr: i64, out_cap: i64, timeout_ms: i64) -> i64       // bytes, 0 = end of input
```

Nothing is inherited from the host: the argument vector and the environment are exactly what the grant carries
(`{"args":["prog","x"],"env":{"K":"V"},"stdin":false}`, `ProcessGrant`). The capability is **deny by default** like
all others, including `arg_count`: a module that imports it without the grant fails with `NotGranted`. The CLI grants it
for you when `--app-arg X` (repeatable; argv[0] is the module file name without extension, then the arguments),
`--env K=V`, `--env-pass K` (copies that one host variable if it is set; never the whole environment) or
`--grant process/v1` is given; standard input needs `--grant process:stdin`. The existing `--arg` (for `--invoke`) and
`--wasi-arg`/`--wasi-env` (WASI) are unchanged. An empty value is defined (`0` length); `=` or NUL in a name is
`ERR_INVALID`. A value that does not fit gives `ERR_TOO_LARGE` with nothing written (query with `out_cap` 0, retry).

`stdin_read` without the stdin grant is `ERR_DENIED`. It returns what is available, at most `out_cap` and 1 MiB
(a short read is not end of input; only `0` is). `timeout_ms < 0` waits without bound, `0` polls, otherwise the call is
`ERR_TIMEOUT` if no byte arrived in time. The CLI reads the process's stdin from a helper thread (shared by all isolates
of the process). Reads block a Tokio worker for the wait, so use a timeout in multi-tenant embeddings.

## 4d. `orbit:net/v1`: TCP, an image of the Orin L0 network layer

Unchanged Orin `std:net` / `std:http` code reaches the network through the L0 functions `orin_platform_tcp_*`,
`orin_platform_stream_recv/send`, `orin_platform_net_*`, `orin_platform_dns_resolve` and `orin_platform_wake_*`. This
capability is the host side of exactly those functions, so the compiler's wasm platform layer is a thin adapter
([ADR-0010](adr/0010-network-capability-mirrors-the-orin-l0-tcp-layer.md)). There is no HTTP in Orbit: HTTP is Orin
library code over TCP.

Conventions: pointers and lengths `i64`; functions return an `i64` (a value `>= 0`, a handle `> 0`, or a negative
status) or an `i32` status; **there are no out-parameters**. Handles are `i64 > 0` (bit 63 never set) from a
per-isolate table: slot index, generation and an instance tag. `wake` arguments name a wake source, `0` = none.
`timeout_ms` is mandatory and bounded: `0..=3600000`, otherwise `ERR_INVALID`; there is no infinite wait.

```
tcp_listen(addr_ptr, addr_len, port: i32, backlog: i32, opt: i32) -> i64   // listener handle. opt bit 0: SO_REUSEADDR. backlog 1..=4096. addr is an IP literal
local_port(h: i64) -> i32                                                   // bound port of a listener or stream (the real one when port was 0)
tcp_accept(listener: i64, timeout_ms: i32, wake: i64) -> i64                // stream handle (TCP_NODELAY on). ERR_TIMEOUT if none arrived
peer(h: i64, out_ptr, out_cap) -> i64                                       // length of the peer address text (numeric host); ERR_TOO_LARGE if out_cap is too small, nothing written
peer_port(h: i64) -> i32
tcp_connect(host_ptr, host_len, port: i32, timeout_ms: i32, wake: i64) -> i64  // stream handle; the timeout bounds the whole connect (all addresses)
recv(h: i64, out_ptr, out_cap, timeout_ms: i32, wake: i64) -> i64          // bytes received, >= 1; ERR_EOF at orderly end; out_cap > 0
send(h: i64, ptr, len, timeout_ms: i32, wake: i64) -> i64                  // bytes accepted, 0..=len (a partial write is normal; the caller sends the tail again)
shutdown(h: i64, how: i32) -> i32                                           // 0 read, 1 write (sends FIN), 2 both
close(h: i64) -> i32                                                        // listener or stream
dns_resolve(name_ptr, name_len, out_ptr, out_cap) -> i64                   // length; numeric addresses of the name, each followed by '\n', duplicates removed, as many as fit; ERR_NOT_FOUND if it does not resolve; ERR_TOO_LARGE if not even one fits
wake_open(parent: i64) -> i64                                               // wake handle; parent 0 = root
wake_signal(h: i64) -> i32                                                  // 0; permanent and idempotent. Also signals every descendant
wake_check(h: i64) -> i32                                                   // 1 if h or an ancestor is set, 0 if not
wake_close(h: i64) -> i32
```

**Authority is the grant.** Grant configuration (all keys optional):
`{"listen": ["127.0.0.1:8080"], "connect": ["*.example.com:443"], "max_sockets": 64, "max_bytes_per_call": 1048576}`.
A scope is `host:port` or `host:lo-hi` (IPv6 literal in brackets). The host part follows the L0 rules: `*` (any),
`*.sfx` (names ending in `.sfx`, not `sfx` itself) or the exact text, ASCII case-insensitive; for `listen` the text is
an address literal. A name or address containing `*` is `ERR_INVALID`. A request outside every scope is
**`ERR_DENIED` before any socket is created or any lookup is made**; with no `listen` / `connect` list everything is
denied. `dns_resolve` needs a `connect` scope whose host pattern covers the name (any port). Scope matching is on the
text the guest passed, not on the resolved address (see [security.md](security.md)). CLI: `--grant
net:listen=127.0.0.1:8080`, `--grant net:connect=*.example.com:443`, `--listen ADDR:PORT`.

**Waits** (`tcp_accept`, `tcp_connect`, `recv`, `send`) are asynchronous host functions: they never occupy an
executor thread. A wait ends with the first of: success, `timeout_ms` (`ERR_TIMEOUT`), the wake source being set
(`ERR_CANCELLED`, also when it was set before the call: that is checked first, so a cancelled task starts no new network
work, and nothing is consumed), and the isolate's **remaining wall-clock deadline** (`Limits::timeout`; the call returns
`ERR_TIMEOUT` and the epoch deadline then ends the guest). A reset connection is `ERR_RESET`, a refused connect
`ERR_REFUSED`, a listen address already in use `ERR_ADDR_IN_USE`; any other OS failure is `ERR_IO` (no other errno is
exposed). `dns_resolve` is bounded by 30 s and, like
`getaddrinfo` in L0, is not interruptible by a wake source once started.

**Wake sources** form a tree like L0's: a source opened under a parent is its child; signalling a parent sets every
descendant, and a source opened under a set parent starts set. The binding of a source to the running task is the guest
runtime's business (it passes the bound handle as `wake`), so the host holds no hidden per-thread state. The host can
also signal from outside the guest (`orbit_caps::NetController`), which works while the isolate is busy in a wait.

**Checking order** (so errors are predictable): handle (`ERR_BAD_HANDLE`: unknown, stale, forged, another instance's, or
the wrong kind), argument ranges and `*`/UTF-8 (`ERR_INVALID`), output buffer bounds (`ERR_INVALID`, before anything is
consumed), scope (`ERR_DENIED`), wake handle (`ERR_BAD_HANDLE`), socket quota (`ERR_QUOTA`), then the operation.

**Quotas**: at most `max_sockets` open sockets per isolate (default 64, listeners and streams together; `ERR_QUOTA`),
and `max_bytes_per_call` (default 1 MiB) per `recv` / `send` (larger requests are served partially, which both calls
allow). At most 4096 live wake sources. Terminating the isolate closes all its sockets. A socket that is closed while a
wait on it is still running in another call is released when that wait ends.

All of it works with 32-bit and 64-bit memories.

## 5. Guest exports

| Export | Required | Meaning |
|---|---|---|
| `memory` | if a pointer-taking import is used | the guest's linear memory |
| `_initialize` | no | called once when the instance is initialised (WASI reactor convention) |
| `_start` | no | WASI command entry point; `orbit run` without `--invoke` calls it and exits with the guest's `proc_exit` code (0 if it returns) |
| `orin_main() -> i32` | no | Orin convention ([orin-integration.md](orin-integration.md) section 2a): the program entry point. `orbit run` without `--invoke` and without `_start` calls it and exits with its result (a non-zero status that wraps to 0 modulo 256 becomes 1) |
| `orin_exit_status() -> i32` | no | Orin convention: after `orin_main` trapped, the exit status the program set, or `-1` for a genuine trap. Read once by `orbit run` (library: `Isolate::post_mortem_i32`) |
| `orbit_init` | no | called once after `_initialize`; a trap or a non-zero `i32` result fails initialisation |
| any function | — | invocable by name with integer arguments |
| `orbit_alloc(size: i64) -> i64` | *reserved* | for host-to-guest payloads in a later version. Servers do not need it: an Orin server is a normal program that listens and accepts through `orbit:net/v1` ([ADR-0010](adr/0010-network-capability-mirrors-the-orin-l0-tcp-layer.md)) |

Orin entry point. `post_mortem_i32(export)` is the only way to run guest code in the `Failed` state. It is allowed only
after an `ExecError::Trap` (not after fuel exhaustion, a deadline, a memory-limit trap, a `proc_exit` or a failed
`orbit_init`), only for an export of exactly the type `() -> i32`, under its own budget (100 000 fuel on a metered
engine, and the invocation timeout capped at 250 ms), and it never leaves `Failed`. It returns `None` if the call itself
traps or exceeds that budget. `orbit run` prints the original trap to stderr and exits with the status when it is not
`-1`; for `-1` (or no export, or a failing call) it reports the original trap and exits 1.

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
