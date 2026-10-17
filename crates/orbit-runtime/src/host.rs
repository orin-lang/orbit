//! Per-isolate host state and the guest-memory helpers capability authors use.

use std::any::Any;
use std::collections::HashMap;
use std::time::Instant;

use orbit_abi::{CapabilityId, MEMORY_EXPORT, status};
use wasmtime::{Caller, ResourceLimiter};

use crate::Limits;
use crate::capability::{InstanceId, InstanceInfo, TenantId};

/// Counters kept by the host while an isolate runs. All values are exact counts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostMetrics {
    /// Host function calls per capability.
    pub calls: HashMap<CapabilityId, u64>,
    /// Bytes accepted by `orbit:log/v1` (messages that were rejected are not counted).
    pub bytes_logged: u64,
}

/// `ResourceLimiter` of an isolate: memory, table and instance limits plus accounting.
#[derive(Debug)]
pub(crate) struct StoreLimiter {
    max_memory: u64,
    max_table_elements: u32,
    pub(crate) memory_bytes: u64,
    pub(crate) peak_memory_bytes: u64,
    /// Denied `memory.grow` requests (including a refused initial allocation).
    pub(crate) memory_grow_denied: u64,
    pub(crate) table_grow_denied: u64,
}

impl StoreLimiter {
    fn new(limits: &Limits) -> Self {
        Self {
            max_memory: limits.max_memory_bytes,
            max_table_elements: limits.max_table_elements,
            memory_bytes: 0,
            peak_memory_bytes: 0,
            memory_grow_denied: 0,
            table_grow_denied: 0,
        }
    }
}

impl ResourceLimiter for StoreLimiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let desired = desired as u64;
        if desired > self.max_memory {
            self.memory_grow_denied += 1;
            return Ok(false);
        }
        self.memory_bytes = desired;
        self.peak_memory_bytes = self.peak_memory_bytes.max(desired);
        Ok(true)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if desired as u64 > u64::from(self.max_table_elements) {
            self.table_grow_denied += 1;
            return Ok(false);
        }
        Ok(true)
    }

    fn instances(&self) -> usize {
        1
    }

    fn memories(&self) -> usize {
        1
    }

    fn tables(&self) -> usize {
        4
    }
}

/// The `T` of the isolate's `wasmtime::Store<T>`.
///
/// Capability implementations receive it through `Caller<'_, HostState>`.
pub struct HostState {
    info: InstanceInfo,
    pub(crate) limiter: StoreLimiter,
    caps: HashMap<CapabilityId, Box<dyn Any + Send>>,
    metrics: HostMetrics,
    /// Absolute deadline of the running invocation, checked by the epoch callback.
    pub(crate) deadline: Option<Instant>,
}

impl HostState {
    pub(crate) fn new(info: InstanceInfo, limits: &Limits) -> Self {
        Self {
            info,
            limiter: StoreLimiter::new(limits),
            caps: HashMap::new(),
            metrics: HostMetrics::default(),
            deadline: None,
        }
    }

    pub(crate) fn insert_cap_state(&mut self, id: CapabilityId, state: Box<dyn Any + Send>) {
        self.caps.insert(id, state);
    }

    pub(crate) fn cap_state_dyn(&mut self, id: &CapabilityId) -> Option<&mut (dyn Any + Send)> {
        self.caps.get_mut(id).map(|b| &mut **b)
    }

    /// Id of this isolate (host-assigned; the guest cannot influence it).
    #[must_use]
    pub fn instance_id(&self) -> InstanceId {
        self.info.instance_id
    }

    /// Tenant of this isolate (host-assigned).
    #[must_use]
    pub fn tenant(&self) -> &TenantId {
        &self.info.tenant
    }

    /// Per-instance info given to capabilities at creation.
    #[must_use]
    pub fn info(&self) -> &InstanceInfo {
        &self.info
    }

    /// The per-instance state of capability `id`, if it has state of type `T`.
    pub fn cap_state_mut<T: Any>(&mut self, id: &CapabilityId) -> Option<&mut T> {
        self.caps.get_mut(id)?.downcast_mut::<T>()
    }

    /// Counts one host call of `id`; call it once at the top of every host function.
    pub fn record_call(&mut self, id: &CapabilityId) {
        if let Some(n) = self.metrics.calls.get_mut(id) {
            *n += 1;
        } else {
            self.metrics.calls.insert(id.clone(), 1);
        }
    }

    /// Adds to the `bytes_logged` counter.
    pub fn add_bytes_logged(&mut self, n: u64) {
        self.metrics.bytes_logged = self.metrics.bytes_logged.saturating_add(n);
    }

    /// Current counters.
    #[must_use]
    pub fn metrics(&self) -> &HostMetrics {
        &self.metrics
    }
}

/// Resolves `(ptr, len)` against the guest's current memory size with checked arithmetic.
fn range(mem_len: usize, ptr: i64, len: i64) -> Result<std::ops::Range<usize>, i32> {
    let (Ok(ptr), Ok(len)) = (u64::try_from(ptr), u64::try_from(len)) else {
        return Err(status::ERR_INVALID);
    };
    let end = ptr.checked_add(len).ok_or(status::ERR_INVALID)?;
    if end > mem_len as u64 {
        return Err(status::ERR_INVALID);
    }
    // `end <= mem_len`, so both conversions are lossless.
    Ok(ptr as usize..end as usize)
}

fn guest_memory(caller: &mut Caller<'_, HostState>) -> Result<wasmtime::Memory, i32> {
    caller
        .get_export(MEMORY_EXPORT)
        .and_then(wasmtime::Extern::into_memory)
        .ok_or(status::ERR_INVALID)
}

/// Copies `len` bytes at `ptr` out of the guest's exported `memory`.
///
/// Works for 32-bit and 64-bit memories. All arithmetic is checked against the memory's
/// size *at the time of the call*.
///
/// # Errors
/// * [`status::ERR_TOO_LARGE`] if `len > max` (checked first, before touching memory);
/// * [`status::ERR_INVALID`] for a negative `ptr`/`len`, an overflowing or out-of-bounds
///   range, or a guest without an exported `memory`.
pub fn read_guest_bytes(
    caller: &mut Caller<'_, HostState>,
    ptr: i64,
    len: i64,
    max: u64,
) -> Result<Vec<u8>, i32> {
    if len >= 0 && len as u64 > max {
        return Err(status::ERR_TOO_LARGE);
    }
    let mem = guest_memory(caller)?;
    let data = mem.data(&*caller);
    let r = range(data.len(), ptr, len)?;
    Ok(data[r].to_vec())
}

/// Copies `bytes` into the guest's exported `memory` at `ptr`.
///
/// # Errors
/// [`status::ERR_INVALID`] for a negative `ptr`, an overflowing or out-of-bounds range, or a
/// guest without an exported `memory`.
pub fn write_guest_bytes(
    caller: &mut Caller<'_, HostState>,
    ptr: i64,
    bytes: &[u8],
) -> Result<(), i32> {
    let mem = guest_memory(caller)?;
    let data = mem.data_mut(&mut *caller);
    let r = range(
        data.len(),
        ptr,
        i64::try_from(bytes.len()).map_err(|_| status::ERR_INVALID)?,
    )?;
    data[r].copy_from_slice(bytes);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_checks() {
        assert_eq!(range(100, 0, 100), Ok(0..100));
        assert_eq!(range(100, 100, 0), Ok(100..100));
        assert_eq!(range(100, 99, 2), Err(status::ERR_INVALID));
        assert_eq!(range(100, -1, 1), Err(status::ERR_INVALID));
        assert_eq!(range(100, 0, -1), Err(status::ERR_INVALID));
        assert_eq!(range(100, i64::MAX, i64::MAX), Err(status::ERR_INVALID));
        assert_eq!(range(100, i64::MAX, 1), Err(status::ERR_INVALID));
    }
}
