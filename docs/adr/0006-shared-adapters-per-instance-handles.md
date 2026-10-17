# ADR-0006: Shared host adapters, per-instance handles

Status: accepted (2026-10-17)

## Context
Capabilities like databases, object stores and brokers need expensive clients. Per-isolate pools would multiply
connections by the number of isolates.

## Decision
* A capability separates its **contract** (the `orbit:*` import module) from an **adapter** trait (`LogSink`,
  `KvStore`, later `SqlPool`, `ObjectStore`, `BrokerClient`). One adapter instance is shared by all isolates of a
  runtime.
* Resources a guest opens are represented by **handles** in a per-instance table (`index + generation`, never a host
  pointer). Closing, terminating or trapping the instance releases them.
* Tenant scoping is applied by the capability before the adapter is called: the tenant id comes from the host's spawn
  call, never from the guest.

## Consequences
Infrastructure clients are shared and bounded per host; the same application binary runs against an in-memory adapter
locally and a real one in a cluster by changing only the host configuration. Adapter bugs can leak across tenants, so
adapters stay small and the tenant key is part of every adapter call.
