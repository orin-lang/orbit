//! Capability implementations for the Orbit runtime.
//!
//! Each capability lives in its own module and implements
//! [`orbit_runtime::Capability`]: [`log`] (`orbit:log/v1`), [`clock`] (`orbit:clock/v1`),
//! [`random`] (`orbit:random/v1`), [`kv`] (`orbit:kv/v1`), [`net`] (`orbit:net/v1`), [`fs`] (`orbit:fs/v1`),
//! [`queue`] (`orbit:queue/v1`), [`process`] (`orbit:process/v1`) and [`wasi`] (`wasi_p1/v1`, the
//! restricted WASI preview 1 shim). To add a
//! capability, create `src/<name>.rs` with a type implementing `Capability`, keep its
//! per-instance data in a state struct returned from `instance_state`, and re-export it here.

pub mod clock;
pub mod fs;
pub mod kv;
pub mod log;
pub mod net;
pub mod process;
pub mod queue;
pub mod random;
pub mod wasi;

pub use clock::{ClockCapability, ManualClock, SystemTimeSource, TimeSource};
pub use fs::{FsCapability, FsGrant, MountMode};
pub use kv::{InMemoryKv, KvCapability, KvError, KvStore};
pub use log::{LogCapability, LogLevel, LogRecord, LogSink, MemorySink, StderrSink};
pub use net::{NetCapability, NetController, NetScope, net_grant_config};
pub use process::{
    HostStdin, MemoryStdin, ProcessCapability, ProcessGrant, StdinError, StdinProvider, StdinReader,
};
pub use queue::{InMemoryQueue, QueueBackend, QueueCapability, QueueError};
pub use random::RandomCapability;
pub use wasi::WasiCapability;
