# ADR-0005: Async isolates with epoch time slices, no thread per isolate

Status: accepted (2026-10-17)

Superseded in part by [ADR-0008](0008-fuel-metering-is-opt-in.md): fuel metering is now opt-in; the epoch time slices below are unchanged.

## Context
Thousands of deployed applications share few cores. Many are idle; active ones mix computation and I/O.

## Options
* A thread per isolate: simple, but memory per thread and scheduler pressure kill density.
* A custom scheduler: premature.
* **Tokio + Wasmtime async**: guest code runs on fibers; host calls are async; epoch interruption makes a running guest
  yield back to the executor at every tick.

## Decision
Isolates are futures on a multi-threaded Tokio runtime. Each invocation gets a fuel budget (deterministic CPU proxy)
and a wall-clock deadline; the epoch deadline callback yields at every tick and traps once the deadline is past.
Admission control (a semaphore for running invocations, a memory budget for live isolates) bounds concurrency and
memory.

## Consequences
An idle isolate costs only its store; a dormant application costs nothing but its compiled artifact. Fairness is
round-robin at tick granularity within each worker thread, with Tokio's work stealing between threads; there is no
priority or weighted fairness yet (roadmap). Deadline precision equals the tick length. Long CPU-bound batch work still
yields, so it cannot starve HTTP-style work on the same threads.
