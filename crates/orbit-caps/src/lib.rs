//! Capability implementations for the Orbit runtime.
//!
//! Each capability lives in its own module and implements
//! [`orbit_runtime::Capability`]. Milestone 1 provides [`log`] (`orbit:log/v1`). To add a
//! capability, create `src/<name>.rs` with a type implementing `Capability`, keep its
//! per-instance data in a state struct returned from `instance_state`, and re-export it here.

pub mod log;

pub use log::{LogCapability, LogLevel, LogRecord, LogSink, MemorySink, StderrSink};
