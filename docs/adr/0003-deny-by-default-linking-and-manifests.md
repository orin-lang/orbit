# ADR-0003: Deny-by-default linking; manifests describe, grants authorise

Status: accepted (2026-10-17)

## Context
Orin knows statically which effects a program may perform (`orin effects`). It is tempting to let that summary decide
what the program may do on Orbit. But the artifact is produced by whoever deploys it, and a compiler check is not a
security property.

## Decision
1. The **only** source of authority is the grant the host passes when it spawns an isolate.
2. The `orbit.manifest` custom section and the import list describe requirements. A requirement without a grant fails
   instantiation; the module never starts.
3. Only granted capabilities are linked. A denied capability is therefore not reachable through any import; there is
   no "linked but returns denied" mode for whole capabilities. (Fine-grained refusals inside a granted capability,
   e.g. a KV namespace outside the grant, return `ERR_DENIED`.)
4. Unknown imports (including WASI and `env.*`) fail at load. Milestone 1 links no WASI at all.
5. If a manifest exists it must list every imported capability (`manifest mismatch` otherwise) so that a tool reading
   only the manifest is never surprised.

## Consequences
Fail-closed and auditable: `orbit inspect` shows exactly what a module needs before anyone grants it. Programs that
optionally use a capability must import it and be granted it (or be built without it); a later "optional" marker in
the manifest can link a deny-stub, decided per capability.
