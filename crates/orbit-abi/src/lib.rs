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

/// Import module name of the restricted WASI preview 1 shim (capability `wasi_p1/v1`).
pub const WASI_P1_IMPORT_MODULE: &str = "wasi_snapshot_preview1";

/// Name of the export a guest must provide for pointer-taking imports.
pub const MEMORY_EXPORT: &str = "memory";
/// Optional export called once when an instance is initialised (WASI reactor convention).
pub const INITIALIZE_EXPORT: &str = "_initialize";
/// Optional export called after `_initialize`; a trap or non-zero `i32` result fails init.
pub const INIT_EXPORT: &str = "orbit_init";

/// Import module and field of the shared linear memory a threaded guest imports
/// (`(import "env" "memory" (memory N 65536 shared))`, `docs/host-abi.md` section 4e).
pub const SHARED_MEMORY_IMPORT: (&str, &str) = ("env", "memory");
/// Export a guest with `orbit:thread/v1` must provide: `(tid: i64, arg: i64)`, no result. The
/// host calls it in every new thread instance.
pub const THREAD_START_EXPORT: &str = "orin_thread_start";

/// Size limits that are part of the ABI contract.
pub mod limits {
    /// Maximum size of one `orbit:log/v1` message.
    pub const LOG_MAX_MESSAGE_BYTES: u64 = 16 * 1024;
    /// Default per-invocation log byte budget.
    pub const LOG_DEFAULT_BUDGET_BYTES: u64 = 1024 * 1024;
    /// Maximum bytes of one `orbit:random/v1` `fill` call.
    pub const RANDOM_MAX_FILL_BYTES: u64 = 64 * 1024;
    /// Maximum bytes of one WASI `random_get` call.
    pub const WASI_RANDOM_MAX_BYTES: u64 = RANDOM_MAX_FILL_BYTES;
    /// Maximum bytes of one WASI `fd_write` call that the host accepts (a larger write is
    /// partial, as POSIX allows).
    pub const WASI_WRITE_MAX_BYTES: u64 = 1024 * 1024;
    /// Maximum number of iovecs in one WASI `fd_write`/`fd_read` call.
    pub const WASI_MAX_IOVS: u32 = 1024;
    /// Maximum KV key size.
    pub const KV_MAX_KEY_BYTES: u64 = 1024;
    /// Maximum KV value size.
    pub const KV_MAX_VALUE_BYTES: u64 = 1024 * 1024;
    /// Default maximum number of open sockets (listeners and streams) per isolate in `orbit:net/v1`.
    pub const NET_DEFAULT_MAX_SOCKETS: usize = 64;
    /// Default maximum bytes moved by one `stream_recv` / `stream_send` call (larger requests
    /// are served partially, which both calls allow).
    pub const NET_DEFAULT_MAX_BYTES_PER_CALL: u64 = 1024 * 1024;
    /// Largest accepted `timeout_ms` of a waiting `orbit:net/v1` call (one hour).
    pub const NET_MAX_TIMEOUT_MS: i32 = 3_600_000;
    /// Maximum number of live wake sources per isolate.
    pub const NET_MAX_WAKE_SOURCES: usize = 4096;
    /// Maximum length of a guest path passed to `orbit:fs/v1`.
    pub const FS_MAX_PATH_BYTES: u64 = 4096;
    /// Maximum bytes moved by one `orbit:fs/v1` or `orbit:process/v1` read or write call
    /// (a larger request is clamped: the call is partial).
    pub const FS_MAX_IO_BYTES: u64 = 1024 * 1024;
    /// Default number of open `orbit:fs/v1` handles (files and directories) per instance.
    pub const FS_DEFAULT_MAX_HANDLES: usize = 64;
    /// Default number of guest threads one isolate may have alive besides its main thread
    /// (`orbit:thread/v1`, grant key `max_threads`).
    pub const THREAD_DEFAULT_MAX_THREADS: usize = 64;
    /// Largest `max_threads` a grant may configure (larger values are clamped).
    pub const THREAD_MAX_THREADS_CEILING: usize = 1024;
    /// Bit set in the result of `fs.dir_next` when the entry is a directory.
    pub const FS_DIR_FLAG: i64 = 0x4000_0000_0000_0000;
}
