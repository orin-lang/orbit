# ADR-0008: Fuel metering is opt-in

Status: accepted (2026-10-17)

## Context
Wasmtime's fuel is an **engine-wide, compile-time** setting (`Config::consume_fuel`). Code compiled with it carries
the accounting on every run, whether or not an isolate's budget is limited. Until now Orbit always enabled it
([ADR-0005](0005-async-isolates-and-epoch-time-slices.md): "a fuel budget (deterministic CPU proxy) and a wall-clock
deadline").

Measured by the Orin compiler's WebAssembly work, 2026-10-17, running Orin programs on Orbit:

| Program | With fuel | Without fuel | Native | Node |
|---|---|---|---|---|
| Orin `fib` | 1.46 s | 0.83 s | 0.36 s | 0.68 s |
| Orin `bintrees` | 1.38 s | 1.15 s | | |

That is roughly 1.8x (`fib`) and 1.2x (`bintrees`) on compute-bound code. Orbit's own measurement on this repository
is in [benchmarks.md](../benchmarks.md). Since the setting is per engine, an unlimited `Limits::fuel = None` does not
avoid the cost.

## Decision
1. `EngineConfig::fuel_metering: bool`, default **false**. The CLI turns it on for a run only with `--fuel N`.
2. With metering off:
   * the CPU bound is the epoch-based wall-clock deadline (`Limits::timeout`); epoch yields still run, so fairness
     between isolates is unchanged;
   * `Limits::fuel = Some(_)` fails at spawn with `InstantiateError::FuelMeteringDisabled`. A fuel budget is never
     silently ignored;
   * fuel metrics are `None` ("not measured"), never 0 (`IsolateMetrics::fuel_consumed`,
     `InvocationMetrics::fuel_consumed`).
3. With metering on, behaviour is exactly as before.
4. Multi-tenant deployments should always set `Limits::timeout`; without metering and without a timeout nothing bounds
   a runaway guest ([security.md](../security.md)).

## Trade-off
* **Fuel** is a deterministic instruction budget: the same input costs the same fuel on every machine and run, which
  makes it usable as a billing proxy and for reproducible limits and tests. It costs about 1.2x to 1.8x on
  compute-heavy code.
* **The epoch deadline** is a wall-clock bound: tick-granular (10 ms by default), and non-deterministic. The same work
  can pass or fail depending on machine load. It measures neither instructions nor CPU time (time spent waiting for a
  run slot or yielded to the executor counts against it).
* Neither is CPU-time accounting.

## Consequences
* Tests that use fuel build a metered runtime.
* A single runtime cannot mix metered and unmetered isolates, because metering is baked into compiled code and the
  artifact cache is keyed by module content only.

## Future
Per-deployment-class engines: one metered engine (billed or reproducible workloads) and one unmetered engine (trusted or
latency-sensitive workloads), sharing the runtime's registry, admission control and memory budget. Compiled artifacts
would then be cached per engine.

Superseded in part: this changes the fuel part of ADR-0005's decision; the epoch time slices there are unchanged.
