# ADR-0011: File system and process capabilities mirror the Orin L0 layer

Status: accepted (2026-10-18)

## Context
Orin programs reach the OS only through the closed list of L0 platform functions (`orin_platform.h`). On Orbit the
wasm L0 answered `EPERM` for every file operation and offered no arguments or environment, so `std:fs` and
`std:process` could not run. The goal is that **unchanged Orin std code runs on Orbit**, and the aim is not to
ship an Orbit-specific library.

## Decision
1. Two capabilities, `orbit:fs/v1` and `orbit:process/v1`, whose functions are the primitives the L0 functions need
   (open/read/write/flush/close, stat, size, directory iteration, realpath, cwd; argv, env, stdin), not a general POSIX
   surface. The compiler's L0 implements `file_read`, `stream_*`, `dir_list`, `path_real`, ... on top of them, so the
   semantics (sticky stream errors, buffering, whole-file reads) stay in the L0 and the host stays small.
2. The authority is **preopened virtual mounts** from the grant. Guest paths are virtual; resolution is lexical
   normalisation, mount matching, then capability-style resolution (`cap-std`) inside the mount, which makes escapes
   impossible by construction (symlinks, `..`) instead of by string checks, and read-only mounts refuse writes before an
   OS call. `cap-std` is a dependency (it uses `unsafe` internally); Orbit crates keep `#![forbid(unsafe_code)]`.
3. Status codes extend the shared table with `ERR_EXISTS`, `ERR_NOT_DIR`, `ERR_IS_DIR` (and the net worker's
   `ERR_EOF`, `ERR_TIMEOUT`, `ERR_RESET`, `ERR_CANCELLED`) so that the L0 can recover `ENOENT`, `EACCES`, `EISDIR`,
   `ENOTDIR` without guessing. `i64`-returning functions return a non-negative value or a status; sizes, handles and
   directory-entry flags travel in the return value, with no out-pointers.
4. `process/v1` is deny by default like every capability, **including args and env**: nothing is inherited from the
   host. The CLI grants it implicitly when `--app-arg`, `--env` or `--env-pass` is given (that is the grant), and stdin
   only with `--grant process:stdin`.
5. Per-instance quotas: open handles, bytes per call, optional total written bytes. Host errors map to status codes; no
   host path or OS text is ever returned.

## Consequences
* A guest can only name what is mounted; the operator decides the mounts. There is no `chdir`; the virtual cwd is fixed
  by the grant (`--cwd`).
* Host file I/O is synchronous on the executor thread; bounded per call, not asynchronous. A future version can move it
  to `spawn_blocking` without changing the ABI.
* Nested mounts, non-UTF-8 names, rename/remove/mkdir, and file metadata beyond kind and size are out of scope until the
  L0 contract needs them (adding functions to `fs/v1` is backwards compatible).
* The L0 maps the statuses back to errno-style codes; the table lives in the compiler's `orbit` platform layer.
