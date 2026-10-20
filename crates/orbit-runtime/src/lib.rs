//! Orbit runtime: WebAssembly isolates with capability-based host access.
//!
//! The pieces, from the outside in:
//!
//! * [`Runtime`] owns an [`OrbitEngine`] (one shared Wasmtime engine plus an epoch ticker), a
//!   [`CapabilityRegistry`], a content-hash cache of compiled [`Artifact`]s and admission
//!   control (a cap on concurrently executing invocations and a memory admission budget).
//! * [`Runtime::load`] validates a module against `docs/host-abi.md` and returns an
//!   [`Artifact`] that records the capabilities it requires.
//! * [`Runtime::spawn_isolate`] checks the requirements against [`Grants`] and creates an
//!   [`Isolate`] with its own store, memory and per-capability state, under [`Limits`].
//! * [`Isolate::init`] / [`Isolate::invoke`] run guest code asynchronously with optional fuel
//!   metering (off by default, see `EngineConfig::fuel_metering`), a wall-clock deadline and cooperative yielding.
//!
//! [`Runtime::pool`] keeps pre-initialised idle isolates ready for one tenant and one grant set
//! ([`InstancePool`]): fresh-per-request by default, optional reuse, scale-to-zero, and LRU
//! eviction under memory pressure ([`Runtime::reclaim_idle`]).
//!
//! Capabilities are implemented against the [`Capability`] trait (see the `orbit-caps` crate);
//! the helpers [`read_guest_bytes`] and [`write_guest_bytes`] give them bounds-checked access
//! to guest memory.
//!
//! The crate forbids `unsafe` code.

mod artifact;
mod capability;
mod engine;
mod error;
mod handles;
mod host;
mod isolate;
mod lifecycle;
mod limits;
mod pool;
mod runtime;
mod shared_mem;
mod thread_cap;
mod threads;

pub use artifact::{
    Artifact, ContentHash, ExportInfo, ImportInfo, ModuleInfo, SharedMemoryDecl, hash_hex,
};
pub use capability::{Capability, CapabilityRegistry, Grants, InstanceId, InstanceInfo, TenantId};
pub use engine::{EngineConfig, OrbitEngine, PoolingConfig};
pub use error::{AdmissionError, EngineError, ExecError, GuestExit, InstantiateError, LoadError};
pub use handles::{HandleTable, MAX_HANDLES, TableFull};
pub use host::{HostMetrics, HostState, check_guest_range, read_guest_bytes, write_guest_bytes};
pub use isolate::{InvocationMetrics, Isolate, IsolateMetrics};
pub use lifecycle::{IsolateState, TransitionError};
pub use limits::Limits;
pub use pool::{InstancePool, PoolConfig, PoolError, PoolStats, PooledIsolate, ReusePolicy};
pub use runtime::{Runtime, RuntimeConfig};
pub use thread_cap::ThreadCapability;
