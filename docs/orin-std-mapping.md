# Orin std on Orbit: L0 to capability mapping

Status: **contract for the Orin wasm platform layer (L0)**. Orbit implements the capabilities named here. The compiler
side (`runtime/platform/orbit/orin_platform_orbit.c` in orin-lang) is being rebuilt against this table; until it lands,
the wasm L0 still returns `EPERM` for files and network and the compiler refuses such programs with E0523.

Goal (Henry, 2026-10-18): **unchanged Orin code runs on Orbit**. A program uses `std:net`, `std:http`, `std:fs`,
`std:process`, `std:time` and `std:task` as it does natively; nothing in Orin imports an Orbit library. The `orbit:*`
imports are only the target of L0, the one layer of the Orin runtime that differs per platform
(`runtime/platform/orin_platform.h`). The host grant, never the module, decides what the program may do.

## Conventions

* Pointers and lengths are i64 (zero-extended on wasm32). Status values are fixed (host-abi.md section 3):
  `OK 0, ERR_INVALID -1, ERR_DENIED -2, ERR_NOT_FOUND -3, ERR_BAD_HANDLE -4, ERR_TOO_LARGE -5, ERR_QUOTA -6, ERR_IO -7,
  ERR_EOF -8, ERR_TIMEOUT -9, ERR_RESET -10, ERR_CANCELLED -11, ERR_EXISTS -12, ERR_NOT_DIR -13, ERR_IS_DIR -14, ERR_REFUSED -15, ERR_ADDR_IN_USE -16`.
* A function returning i64 returns a value >= 0 (a handle is > 0) or one of those negative statuses.
* An output buffer that is too small gives `ERR_TOO_LARGE` and nothing is written (grow and call again).
* read, write, recv, send and stdin_read move at most 1 MiB per call; a short result is normal, L0 loops.
* A capability the program never reaches is not imported (wasm-ld drops dead imports), so it needs no grant.

## std module to L0 to Orbit

| std module | L0 functions | Orbit import | Grant (`orbit run`) | State |
|---|---|---|---|---|
| `std:io`, `Console` | write, write_line, flush | `orbit:log/v1` write | `--grant log/v1` | done |
| `std:time` | now_millis, monotonic_millis | `orbit:clock/v1` | `--grant clock/v1` | done |
| `std:process` args | arg_count, arg | `orbit:process/v1` arg_count, arg | `--app-arg X` | Orbit done, L0 pending |
| `std:process` env | getenv | `orbit:process/v1` env_get | `--env K=V`, `--env-pass K` | Orbit done, L0 pending |
| `std:io` stdin | stdin_read, stdin_getc | `orbit:process/v1` stdin_read | `--grant process:stdin` | Orbit done, L0 pending |
| `std:process` exit | exit, at_exit, abort | export `orin_exit_status` | none | done |
| `std:fs` | file_read, file_write, file_exists, dir_list, stream_* | `orbit:fs/v1` | `--grant fs:/data=./dir:ro\|rw`, `--cwd` | Orbit done, L0 pending |
| `std:path` | path_absolute, path_real | `orbit:fs/v1` cwd, realpath | (with fs) | Orbit done, L0 pending |
| `std:net`, `std:http` | net_narrow, tcp_listen, tcp_accept, tcp_connect, stream_recv, stream_send, net_shutdown, net_close, dns_resolve | `orbit:net/v1` | `--grant net:listen=127.0.0.1:8080`, `--grant net:connect=*.example.com:443`, `--listen` | Orbit done, L0 pending |
| `std:task` cancellation | wake_open, wake_close, wake_signal, wake_check, wake_bind, wake_bound | `orbit:net/v1` wake_* | (with net) | Orbit done, L0 pending |
| `std:task` tasks | thread_create, thread_join, thread_exit, thread_id, cpu_count | `orbit:thread/v1` spawn + export `orin_thread_start` | `--grant thread/v1` | **proposed, not built** |
| `std:task` monitors | monitor_* | `memory.atomic.wait32` / `notify` on shared memory, no import | none | **proposed, L0 only** |
| `std:host` shell | shell, shell_timeout | none | none | not available: returns -1 |

## Signatures

`orbit:fs/v1` (host-abi.md section 4b):

```
open(path_ptr, path_len, mode:i32) -> i64      mode 0 read, 1 write/create/truncate, 2 append/create
read(h, out_ptr, out_cap) -> i64               0 = end of file
write(h, ptr, len) -> i64
flush(h) -> i32        close(h) -> i32
stat(path_ptr, path_len) -> i32                0 none, 1 file, 2 dir
size(path_ptr, path_len) -> i64
dir_open(path_ptr, path_len) -> i64
dir_next(h, out_ptr, out_cap) -> i64           name length, bit 62 set for a directory, 0 = end
dir_close(h) -> i32
realpath(path_ptr, path_len, out_ptr, out_cap) -> i64   virtual path, never a host path
cwd(out_ptr, out_cap) -> i64
```

`orbit:process/v1` (section 4c):

```
arg_count() -> i32
arg(i:i32, out_ptr, out_cap) -> i64
env_get(name_ptr, name_len, out_ptr, out_cap) -> i64     ERR_NOT_FOUND when unset
stdin_read(out_ptr, out_cap, timeout_ms:i64) -> i64      0 = EOF; timeout < 0 waits forever
```

`orbit:net/v1` (section 4d):

```
tcp_listen(addr_ptr, addr_len, port:i32, backlog:i32, opt:i32) -> i64     opt bit 0 = SO_REUSEADDR
local_port(h) -> i32
tcp_accept(listener, timeout_ms:i32, wake:i64) -> i64
peer(h, out_ptr, out_cap) -> i64       peer_port(h) -> i32
tcp_connect(host_ptr, host_len, port:i32, timeout_ms:i32, wake:i64) -> i64
recv(h, out_ptr, out_cap, timeout_ms:i32, wake:i64) -> i64     >= 1, or ERR_EOF
send(h, ptr, len, timeout_ms:i32, wake:i64) -> i64
shutdown(h, how:i32) -> i32            close(h) -> i32
dns_resolve(name_ptr, name_len, out_ptr, out_cap) -> i64      "addr\n" lines
wake_open(parent:i64) -> i64           wake_signal(h) -> i32    wake_check(h) -> i32 (1/0)    wake_close(h) -> i32
```

`wake` is 0 or a wake handle. L0 keeps `wake_bind` / `wake_bound` itself and passes the bound handle to every waiting call.

`orbit:thread/v1` (proposed, agreed with the Orin wasm thread, not yet built in Orbit):

```
spawn(arg:i64) -> i64                  tid > 0 or a status
guest export orin_thread_start(tid:i64, arg:i64)
```

The module imports its memory as shared `env.memory` (wasm32, declared max 65536 pages); Orbit provides a
`SharedMemory` whose max is the isolate's memory limit. Each thread is a separate instance on that memory; exit or a
trap in any thread ends all of them. Net, fs and process state is per isolate and shared by its threads. Modules without
threads (exported, unshared memory) stay valid and keep the run-on-block scheduler.

## Queues: not part of std (`orbit:queue/v1`)

Publishing to a message queue is a platform service, not a std module, so it has **no row in the table above** and
no L0 function: `std:*` stays portable and an unchanged program keeps running natively. The binding is a small
separate Orin package (working name `orbit` or `orbit-queue`) that declares the two imports itself and wraps them:

```ts
// package: orbit-queue (std-less; only compiles for wasm32-orbit)
extern "orbit:queue/v1" function publish(q: *u8, qlen: i64, body: *u8, blen: i64): i32;

export function publish(queue: string, body: Bytes): Result<(), QueueError> { /* map status -> QueueError */ }
export function publishBatch(queue: string, bodies: Array<Bytes>): Result<(), QueueError> { /* u32 LE framing */ }
```

Status mapping: `ERR_DENIED` to `QueueError.Denied(queue)`, `ERR_QUOTA` to `Full`, `ERR_TOO_LARGE`/`ERR_INVALID` to
`Invalid`, `ERR_IO` to `Backend`. Grant: `orbit run --grant queue/v1 --publish-queue orders`, or in orbitd's app spec
`"publish": ["orders"]`. The exact extern syntax depends on how the compiler exposes raw imports to packages; the
contract is the signatures in host-abi.md section 4e. A host that does not grant `queue/v1` fails the module at load,
so a program that links the package only runs where queues exist.

## L0 implementation notes

* `file_read`: size, open(0), read until 0, close. `file_write`: open(1), write loop, flush, close; first error wins.
* `file_exists`: stat is 1 or 2. `dir_list`: dir_open, dir_next until 0 (grow the buffer on ERR_TOO_LARGE), dir_close.
* `path_absolute`: cwd once, join, then L0's own normalize. `path_real`: realpath of the normalized path.
* `arg`: call with cap 0 for the length, allocate once at init. `getenv`: ERR_NOT_FOUND gives NULL.
* `tcp_listen`: then `local_port` for `*out_port`. `tcp_accept`: then `peer` and `peer_port`. NODELAY is always on.
* `net_narrow` stays pure in L0; the host grant is the outer bound and is checked again on every call.
* `wake_open` maps a negative result to 0 (table full); `wake_check` maps a negative result to 2 (not a live handle).

Status back to L0 codes:

| Orbit | net functions | file and process functions |
|---|---|---|
| `ERR_DENIED` -2 | `ORIN_PLATFORM_EPERM` | `EACCES` |
| `ERR_BAD_HANDLE` -4 | `ORIN_PLATFORM_EPERM` | `EBADF` |
| `ERR_EOF` -8 | `ORIN_PLATFORM_NET_EOF` | not produced |
| `ERR_TIMEOUT` -9 | `ORIN_PLATFORM_NET_TIMEOUT` | not produced with timeout -1 |
| `ERR_RESET` -10 | `ORIN_PLATFORM_NET_RESET` | not produced |
| `ERR_CANCELLED` -11 | `ORIN_PLATFORM_NET_CANCELLED` | not produced |
| `ERR_NOT_FOUND` -3 | `ENOENT` (DNS miss) | `ENOENT` |
| `ERR_NOT_DIR` -13, `ERR_IS_DIR` -14, `ERR_EXISTS` -12 | not produced | `ENOTDIR`, `EISDIR`, `EEXIST` |
| `ERR_INVALID` -1 | `EINVAL` | `EINVAL` |
| `ERR_TOO_LARGE` -5 | grow and retry | `ENAMETOOLONG` for paths, else grow and retry |
| `ERR_QUOTA` -6 | `EMFILE` | `EMFILE` (handles), `ENOSPC` (write quota) |
| `ERR_REFUSED` -15 | `ECONNREFUSED` | not produced |
| `ERR_ADDR_IN_USE` -16 | `EADDRINUSE` | not produced |
| `ERR_IO` -7 | `EIO` | `EIO` |

The host never returns error text; `orin_platform_error_message` stays in L0.

## Not available

mkdir, remove, rename, chmod, processes (`shell`), UDP, TLS, and a poll over several sockets. A single-threaded guest
serves one connection at a time until `orbit:thread/v1` exists.
