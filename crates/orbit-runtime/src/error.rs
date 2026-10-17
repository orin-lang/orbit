//! Typed errors of the runtime.

use orbit_abi::{CapabilityId, ManifestError};

use crate::IsolateState;

/// Why a module could not be loaded as an [`Artifact`](crate::Artifact).
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// The bytes are neither valid WebAssembly nor valid WAT.
    #[error("invalid module: {0}")]
    Invalid(String),
    /// Compilation failed.
    #[error("compilation failed: {0}")]
    Compile(String),
    /// The module imports something that is not part of the host ABI (`env.foo`, a WASI
    /// function the shim does not implement, an unknown capability/major version or function).
    #[error("unknown import `{module}.{name}`")]
    UnknownImport {
        /// Import module name.
        module: String,
        /// Import field name.
        name: String,
    },
    /// The module imports a memory, table, global or tag; only functions are allowed.
    #[error("import `{module}.{name}` is a {kind}; only function imports are allowed")]
    NonFunctionImport {
        /// Import module name.
        module: String,
        /// Import field name.
        name: String,
        /// Kind of the import (`memory`, `table`, ...).
        kind: &'static str,
    },
    /// The import's signature differs from the ABI definition.
    #[error("signature mismatch for `{module}.{name}`: expected {expected}, found {found}")]
    SignatureMismatch {
        /// Import module name.
        module: String,
        /// Import field name.
        name: String,
        /// Signature required by the ABI.
        expected: String,
        /// Signature declared by the module.
        found: String,
    },
    /// The module uses pointer-taking imports but does not export `memory`.
    #[error("module imports `{module}.{name}` which needs guest memory, but exports no `memory`")]
    MissingMemoryExport {
        /// Import module name.
        module: String,
        /// Import field name.
        name: String,
    },
    /// The module imports a capability that is defined for 32-bit memories only (WASI
    /// preview 1) but its exported `memory` is a 64-bit memory.
    #[error(
        "import `{module}.{name}` is wasm32-only (its pointers are i32) but the module's exported `memory` is a 64-bit memory"
    )]
    Memory64Unsupported {
        /// Import module name.
        module: String,
        /// Import field name.
        name: String,
    },
    /// The `orbit.manifest` section is malformed.
    #[error("invalid manifest: {0}")]
    Manifest(#[from] ManifestError),
    /// The module has more than one `orbit.manifest` section.
    #[error("invalid manifest: more than one `orbit.manifest` section")]
    DuplicateManifest,
    /// Capabilities are imported but missing from the manifest.
    #[error("manifest mismatch: imported but not listed in the manifest: {}", join(.missing))]
    ManifestMismatch {
        /// Capabilities imported by the module but absent from `requires`.
        missing: Vec<CapabilityId>,
    },
    /// The capability is part of the ABI but this host does not serve it.
    #[error("capability {0} is not available on this host")]
    UnsupportedCapability(CapabilityId),
    /// Linking against the registered capabilities failed.
    #[error("link error: {0}")]
    Link(String),
}

fn join(ids: &[CapabilityId]) -> String {
    ids.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Refusal by admission control.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdmissionError {
    /// Granting this isolate's memory limit would exceed the runtime's memory budget.
    #[error("memory admission budget exceeded: requested {requested} bytes, {available} available")]
    MemoryBudget {
        /// Bytes asked for.
        requested: u64,
        /// Bytes still unreserved.
        available: u64,
    },
    /// The runtime's invocation semaphore was closed.
    #[error("runtime is shut down")]
    Closed,
}

/// Why an isolate could not be created.
#[derive(Debug, thiserror::Error)]
pub enum InstantiateError {
    /// The artifact requires a capability the grants do not include. Nothing ran.
    #[error("capability not granted: {0}")]
    NotGranted(CapabilityId),
    /// Granted and required, but the host has no implementation registered.
    #[error("capability not available on this host: {0}")]
    Unavailable(CapabilityId),
    /// Admission control refused the isolate.
    #[error(transparent)]
    Admission(#[from] AdmissionError),
    /// The module's declared memory or tables exceed the isolate's limits.
    #[error("resource limit exceeded at instantiation: {0}")]
    ResourceLimit(String),
    /// `Limits::fuel` is set but the runtime's engine was built without fuel metering
    /// (`EngineConfig::fuel_metering = false`). A fuel budget is never silently ignored.
    #[error(
        "Limits::fuel is set but fuel metering is disabled in this runtime (EngineConfig::fuel_metering)"
    )]
    FuelMeteringDisabled,
    /// Instantiation failed for another reason (for example the start function trapped).
    #[error("instantiation failed: {0}")]
    Instantiate(String),
}

/// Why an initialisation or invocation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExecError {
    /// The isolate's state does not allow the operation.
    #[error("isolate is {0:?}; operation not allowed")]
    InvalidState(IsolateState),
    /// The module has no exported function with this name.
    #[error("no exported function `{0}`")]
    NoSuchExport(String),
    /// The arguments do not fit the function's signature.
    #[error("bad arguments: {0}")]
    BadArguments(String),
    /// The invocation used up its fuel.
    #[error("out of fuel")]
    OutOfFuel,
    /// The wall-clock deadline of the invocation passed.
    #[error("deadline exceeded")]
    DeadlineExceeded,
    /// The guest trapped after a `memory.grow`/`table.grow` was denied by the limits.
    #[error("memory limit: guest trapped after {denied} denied growth request(s): {trap}")]
    MemoryLimit {
        /// Number of denied growth requests during the failing invocation.
        denied: u64,
        /// The trap that followed.
        trap: String,
    },
    /// The guest trapped.
    #[error("trap: {0}")]
    Trap(String),
    /// `orbit_init` returned a non-zero status.
    #[error("orbit_init returned {0}")]
    InitFailed(i32),
    /// The guest called `proc_exit` (WASI) with a non-zero code. `proc_exit(0)` is not an
    /// error: the call returns `Ok` and [`Isolate::exit_code`](crate::Isolate::exit_code)
    /// is `Some(0)`.
    #[error("guest exited with code {0}")]
    Exit(i32),
    /// The runtime was shut down while the invocation waited for a slot.
    #[error("runtime is shut down")]
    Closed,
}

/// The error a host function returns to end the current invocation with an exit code.
///
/// `orbit-caps`' WASI shim returns `Err(wasmtime::Error::new(GuestExit(code)))` from
/// `proc_exit`; the isolate recognises it and reports [`ExecError::Exit`] (or success for
/// code 0) instead of a trap. Wasmtime unwinds the guest stack when a host function errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuestExit(pub i32);

impl std::fmt::Display for GuestExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "guest exited with code {}", self.0)
    }
}

impl std::error::Error for GuestExit {}

/// The engine could not be created (invalid configuration or platform limits).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cannot create engine: {0}")]
pub struct EngineError(pub String);
