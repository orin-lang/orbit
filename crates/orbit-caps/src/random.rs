//! `orbit:random/v1`: `fill(ptr: i64, len: i64) -> i32`, backed by the OS CSPRNG.

use std::any::Any;

use orbit_abi::{CapabilityId, limits::RANDOM_MAX_FILL_BYTES, status};
use orbit_runtime::{Capability, HostState, InstanceInfo, write_guest_bytes};
use wasmtime::{Caller, Linker};

/// The `orbit:random/v1` capability. Stateless.
#[derive(Clone, Copy, Debug, Default)]
pub struct RandomCapability;

impl RandomCapability {
    /// A random capability.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

fn fill(id: &CapabilityId, caller: &mut Caller<'_, HostState>, ptr: i64, len: i64) -> i32 {
    caller.data_mut().record_call(id);
    if len < 0 {
        return status::ERR_INVALID;
    }
    if len as u64 > RANDOM_MAX_FILL_BYTES {
        return status::ERR_TOO_LARGE;
    }
    // Bounds are validated by `write_guest_bytes`; generate into a host buffer first so a
    // rejected range never consumes entropy visible to the guest.
    let mut buf = vec![0u8; len as usize];
    if getrandom::fill(&mut buf).is_err() {
        return status::ERR_IO;
    }
    match write_guest_bytes(caller, ptr, &buf) {
        Ok(()) => status::OK,
        Err(code) => code,
    }
}

impl Capability for RandomCapability {
    fn id(&self) -> CapabilityId {
        CapabilityId::random_v1()
    }

    fn link(&self, linker: &mut Linker<HostState>) -> wasmtime::Result<()> {
        let id = self.id();
        let module = id.import_module();
        linker.func_wrap(
            &module,
            "fill",
            move |mut caller: Caller<'_, HostState>, ptr: i64, len: i64| -> i32 {
                fill(&id, &mut caller, ptr, len)
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
