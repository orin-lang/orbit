//! Capability implementations for the Orbit runtime.
//!
//! Each capability lives in its own module and implements
//! [`orbit_runtime::Capability`]: [`log`] (`orbit:log/v1`), [`clock`] (`orbit:clock/v1`),
//! [`random`] (`orbit:random/v1`), [`kv`] (`orbit:kv/v1`) and [`wasi`] (`wasi_p1/v1`, the
//! restricted WASI preview 1 shim). To add a
//! capability, create `src/<name>.rs` with a type implementing `Capability`, keep its
//! per-instance data in a state struct returned from `instance_state`, and re-export it here.

pub mod clock;
pub mod kv;
pub mod log;
pub mod random;
pub mod wasi;

pub use clock::{ClockCapability, ManualClock, SystemTimeSource, TimeSource};
pub use kv::{InMemoryKv, KvCapability, KvError, KvStore};
pub use log::{LogCapability, LogLevel, LogRecord, LogSink, MemorySink, StderrSink};
pub use random::RandomCapability;
pub use wasi::WasiCapability;
