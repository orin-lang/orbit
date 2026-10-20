//! The `orbit:thread/v1` capability: `spawn(arg: i64) -> i64`.

use std::any::Any;

use orbit_abi::{CapabilityId, status};
use wasmtime::{Caller, Linker};

use crate::capability::{Capability, InstanceInfo};
use crate::host::HostState;

/// `orbit:thread/v1` (`docs/host-abi.md` section 4f, ADR-0012).
///
/// `spawn(arg)` starts a new guest thread: a new instance of the same module on the isolate's
/// shared memory, which runs the export `orin_thread_start(tid: i64, arg: i64)` on its own OS
/// thread. It returns the thread id (`> 0`) or a negative status: `ERR_QUOTA` when the
/// isolate already has `max_threads` live threads (grant configuration
/// `{"max_threads": N}`, default 64), `ERR_DENIED` when `max_threads` is 0, `ERR_CANCELLED`
/// when the isolate is already terminating. Without a grant the isolate cannot be created
/// (`InstantiateError::NotGranted`), as for every capability.
///
/// The thread machinery lives in the runtime (it creates stores); this type only exposes the
/// import. Register it like any capability: `registry.register(ThreadCapability::new())`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ThreadCapability;

impl ThreadCapability {
    /// The capability.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Capability for ThreadCapability {
    fn id(&self) -> CapabilityId {
        CapabilityId::thread_v1()
    }

    fn link(&self, linker: &mut Linker<HostState>) -> wasmtime::Result<()> {
        let id = self.id();
        let module = id.import_module();
        linker.func_wrap(
            &module,
            "spawn",
            move |mut caller: Caller<'_, HostState>, arg: i64| -> i64 {
                caller.data_mut().record_call(&id);
                match caller.data().threads.clone() {
                    Some(t) => t.spawn(caller.data_mut(), arg),
                    None => i64::from(status::ERR_DENIED),
                }
            },
        )?;
        Ok(())
    }

    fn instance_state(
        &self,
        _info: &InstanceInfo,
        _config: &serde_json::Value,
    ) -> Box<dyn Any + Send> {
        Box::new(())
    }
}
