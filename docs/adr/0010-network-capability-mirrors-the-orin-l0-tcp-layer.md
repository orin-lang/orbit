# ADR-0010: Network capability mirrors the Orin L0 TCP layer

Status: accepted (2026-10-18)

## Context
The first plan for milestone 4 was an event ABI: guests export `orbit_alloc` and `orbit_on_event`, and Orbit ships an
`orbit:http/v1` capability and an HTTP server that dispatches each request to an isolate. That asks every program to
import an Orbit library and to be written as an event handler.

The requirement is different: **unchanged Orin code (`std:net`, `std:http`, `std:fs`, ...) must run on Orbit.** Orin
reaches the operating system only through its L0 platform layer (`runtime/platform/orin_platform.h`: a closed list of
primitives, with a blocking TCP API, wake sources for cancellation and a scope-based grant). Programs are ordinary
programs: they listen, accept, read and write. The `orbit:*` imports are only the target of the compiler's wasm L0.

## Decision
1. The event plan (`orbit_alloc` / `orbit_on_event`, `dispatch_event`, `orbit:http/v1`, an HTTP server crate) is dropped.
   HTTP is Orin library code on top of TCP.
2. `orbit:net/v1` ([host-abi.md](../host-abi.md) section 4d) mirrors the L0 TCP functions one to one: listen, accept,
   connect, recv, send, shutdown, close, DNS resolve and wake sources. Orbit conventions apply (`i64` pointers and
   lengths, status codes); the L0 out-parameters become return values, and four statuses are added (`ERR_EOF`,
   `ERR_TIMEOUT`, `ERR_RESET`, `ERR_CANCELLED`).
3. Authority is the grant, using the L0 scope rules (`*`, `*.suffix`, exact; port ranges), checked before any socket
   exists. Per-isolate quotas bound open sockets and bytes per call.
4. Waiting calls are real async host functions (Tokio), so thousands of isolates blocked in `accept` or `recv` cost no
   threads. A wake source is a host-side `Notify`-backed token tree, so `wake_signal` interrupts a wait in another call
   and later in another guest thread. All of an isolate's network state is one `Arc`, not per `Store`, so a future
   `orbit:thread/v1` can share it between the wasm instances of one isolate.
5. Wall-clock deadline: a server legitimately waits. Waits are clamped to the isolate's remaining deadline, so a deadline
   is never overshot by a wait; a server runs with no `Limits::timeout` (CLI: no flag, or `--timeout-ms 0`) and keeps
   the CPU bounds that do not conflict with waiting: fuel (opt-in) and the memory limits. A deadline-bounded server would
   need an invocation model with per-request deadlines, which is the pooling work of milestone 3.

## Consequences
* An Orin HTTP server works with no Orbit-specific code; the compiler team reimplements the wasm L0 network functions on
  these imports (list in the report that accompanied this change).
* Connection admission, request limits and TLS are the program's business, not Orbit's. Orbit only bounds sockets, bytes
  per call and the time of each wait.
* Scopes match on the text the guest passes. A name allowed by `*.example.com` that resolves to a private address is
  reachable: the same limitation as L0 (security.md).
* `orbit:net/v1` has no UDP, no TLS, no listener-side DNS and no `poll`/select over several sockets yet: a
  single-threaded guest serves one connection at a time until `orbit:thread/v1` exists.
* `Isolate::cap_state_mut`, `HostState::deadline_remaining`, `check_guest_range` and `Capability`-independent
  `NetController` are the small host-side additions.
