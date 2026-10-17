# Orbit host ABI, version 1

Status: **implemented in milestone 1** for `orbit:log/v1`, `orbit:clock/v1`, `orbit:random/v1` and `orbit:kv/v1`.
Everything marked *reserved* is a design, not a feature.

This document is the contract between a guest module (an Orin program compiled to WebAssembly, or any other module)
and an Orbit host. It is deliberately a **core-WebAssembly** ABI (plain imports and exports with integer parameters),
not a Component Model world; [ADR-0002](adr/0002-core-wasm-abi-before-component-model.md) explains why and how a WIT
version can follow.

## 1. Versioning

* The ABI as a whole has a version (`ORBIT_ABI_VERSION = 1`, crate `orbit-abi`).
* Every capability is versioned on its own. Its import module name is `orbit:<capability>/v<major>`, for example
  `orbit:log/v1`. A host may serve several majors of one capability at once; a guest imports exactly one.
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

Keys at most 1 KiB, values at most 1 MiB. The namespaces an instance may open are part of its grant; the storage of a
namespace is scoped by tenant, so two tenants opening `"cache"` never see each other's keys.

## 5. Guest exports

| Export | Required | Meaning |
|---|---|---|
| `memory` | if a pointer-taking import is used | the guest's linear memory |
| `_initialize` | no | called once when the instance is initialised (WASI reactor convention) |
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
2. rejects every import whose module is not an `orbit:` capability the host knows at that major version
   (this rejects `wasi_snapshot_preview1` and arbitrary `env` imports in milestone 1);
3. rejects imports that are not functions (imported memories, tables and globals are not part of ABI v1);
4. checks the import's signature against the capability's definition;
5. at instantiation, checks that the instance's grant includes every required capability — a denied capability
   fails instantiation (`capability not granted`); the module never runs.

So a denied capability is not reachable through any supported interface: its functions are not linked.
