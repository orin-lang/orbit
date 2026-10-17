//! Orbit host ABI definitions (version 1).
//!
//! This crate is the machine-readable form of `docs/host-abi.md`. It has no dependency on
//! any WebAssembly engine, so guest tooling (compilers, linters) can share it with the host.
//!
//! It defines:
//! * [`ABI_VERSION`] and the [`status`] codes returned by host functions,
//! * [`CapabilityId`], the identity of a versioned capability (`orbit:log/v1` as an import
//!   module name, `log/v1` in a manifest),
//! * a static table of function signatures per capability ([`CAPABILITIES`], [`spec`]),
//! * the [`Manifest`] carried in the `orbit.manifest` custom section.

mod capability;
mod manifest;
pub mod status;

pub use capability::{
    CAPABILITIES, CapabilityId, CapabilitySpec, FuncSig, IdParseError, ValKind, import_module, spec,
};
pub use manifest::{MANIFEST_SECTION, Manifest, ManifestError};

/// Version of the Orbit host ABI as a whole.
pub const ABI_VERSION: u32 = 1;

/// Prefix of every capability import module name.
pub const IMPORT_PREFIX: &str = "orbit:";

/// Name of the export a guest must provide for pointer-taking imports.
pub const MEMORY_EXPORT: &str = "memory";
/// Optional export called once when an instance is initialised (WASI reactor convention).
pub const INITIALIZE_EXPORT: &str = "_initialize";
/// Optional export called after `_initialize`; a trap or non-zero `i32` result fails init.
pub const INIT_EXPORT: &str = "orbit_init";

/// Size limits that are part of the ABI contract.
pub mod limits {
    /// Maximum size of one `orbit:log/v1` message.
    pub const LOG_MAX_MESSAGE_BYTES: u64 = 16 * 1024;
    /// Default per-invocation log byte budget.
    pub const LOG_DEFAULT_BUDGET_BYTES: u64 = 1024 * 1024;
    /// Maximum bytes of one `orbit:random/v1` `fill` call.
    pub const RANDOM_MAX_FILL_BYTES: u64 = 64 * 1024;
    /// Maximum KV key size.
    pub const KV_MAX_KEY_BYTES: u64 = 1024;
    /// Maximum KV value size.
    pub const KV_MAX_VALUE_BYTES: u64 = 1024 * 1024;
}
