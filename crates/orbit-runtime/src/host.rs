//! Per-isolate host state and the guest-memory helpers capability authors use.

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use orbit_abi::{CapabilityId, MEMORY_EXPORT, status};
use wasmtime::{Caller, ResourceLimiter, SharedMemory};

use crate::Limits;
use crate::capability::{InstanceId, InstanceInfo, TenantId};
use crate::threads::ThreadShared;

/// Counters kept by the host while an isolate runs. All values are exact counts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostMetrics {
    /// Host function calls per capability.
    pub calls: HashMap<CapabilityId, u64>,
    /// Bytes accepted by `orbit:log/v1` and by the WASI shim's stdout/stderr (rejected
    /// messages are not counted).
    pub bytes_logged: u64,
}

impl HostMetrics {
    /// Adds the counters of `other` (the metrics of a finished guest thread) to these.
    pub(crate) fn merge(&mut self, other: &HostMetrics) {
        for (id, n) in &other.calls {
            *self.calls.entry(id.clone()).or_insert(0) += n;
        }
        self.bytes_logged = self.bytes_logged.saturating_add(other.bytes_logged);
    }
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
    /// The isolate's thread state (`Some` for a module that imports a shared memory).
    pub(crate) threads: Option<Arc<ThreadShared>>,
    /// The shared linear memory the host functions read and write instead of an exported
    /// `memory` (same value as `threads.memory`).
    pub(crate) shared_memory: Option<SharedMemory>,
    /// Whether the epoch callback ends this store when the isolate is terminating. `false`
    /// for the main store outside an invocation (the post-mortem export must be able to run).
    pub(crate) observe_termination: bool,
}

impl HostState {
    pub(crate) fn new(info: InstanceInfo, limits: &Limits) -> Self {
        Self {
            info,
            limiter: StoreLimiter::new(limits),
            caps: HashMap::new(),
            metrics: HostMetrics::default(),
            deadline: None,
            threads: None,
            shared_memory: None,
            observe_termination: false,
        }
    }

    pub(crate) fn insert_cap_state(&mut self, id: CapabilityId, state: Box<dyn Any + Send>) {
        self.caps.insert(id, state);
    }

    pub(crate) fn cap_state_dyn(&mut self, id: &CapabilityId) -> Option<&mut (dyn Any + Send)> {
        self.caps.get_mut(id).map(|b| &mut **b)
    }

    /// Time left until the running invocation's wall-clock deadline (`Limits::timeout`), or
    /// `None` when there is no deadline. Long-waiting host functions clamp their waits to it.
    #[must_use]
    pub fn deadline_remaining(&self) -> Option<std::time::Duration> {
        self.deadline
            .map(|d| d.saturating_duration_since(Instant::now()))
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

/// The guest's linear memory as the host functions see it: an exported plain `memory`, or the
/// shared memory the isolate provided to a threaded module.
enum GuestMem {
    Plain(wasmtime::Memory),
    Shared(SharedMemory),
}

fn guest_memory(caller: &mut Caller<'_, HostState>) -> Result<GuestMem, i32> {
    if let Some(m) = &caller.data().shared_memory {
        return Ok(GuestMem::Shared(m.clone()));
    }
    caller
        .get_export(MEMORY_EXPORT)
        .and_then(wasmtime::Extern::into_memory)
        .map(GuestMem::Plain)
        .ok_or(status::ERR_INVALID)
}

impl GuestMem {
    /// Current size in bytes.
    fn len(&self, caller: &Caller<'_, HostState>) -> usize {
        match self {
            GuestMem::Plain(m) => m.data_size(caller),
            GuestMem::Shared(m) => m.data_size(),
        }
    }
}

/// Copies `len` bytes at `ptr` out of the guest's memory (the exported `memory`, or the shared
/// memory of a threaded module).
///
/// Works for 32-bit and 64-bit memories. All arithmetic is checked against the memory's
/// size *at the time of the call*.
///
/// # Errors
/// * [`status::ERR_TOO_LARGE`] if `len > max` (checked first, before touching memory);
/// * [`status::ERR_INVALID`] for a negative `ptr`/`len`, an overflowing or out-of-bounds
///   range, or a guest without a memory.
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
    match mem {
        GuestMem::Plain(m) => {
            let data = m.data(&*caller);
            let r = range(data.len(), ptr, len)?;
            Ok(data[r].to_vec())
        }
        GuestMem::Shared(m) => {
            let r = range(m.data_size(), ptr, len)?;
            let mut out = vec![0u8; r.len()];
            if crate::shared_mem::read(&m, r.start, &mut out) {
                Ok(out)
            } else {
                Err(status::ERR_INVALID)
            }
        }
    }
}

/// Checks that `(ptr, len)` lies inside the guest's memory right now, without copying
/// anything. Long-running host functions call it *before* they wait or consume input, so a
/// bad output pointer is refused while nothing has happened yet.
///
/// # Errors
/// [`status::ERR_INVALID`] as for [`read_guest_bytes`].
pub fn check_guest_range(
    caller: &mut Caller<'_, HostState>,
    ptr: i64,
    len: i64,
) -> Result<(), i32> {
    let mem = guest_memory(caller)?;
    range(mem.len(caller), ptr, len).map(|_| ())
}

/// Copies `bytes` into the guest's memory at `ptr`.
///
/// # Errors
/// [`status::ERR_INVALID`] for a negative `ptr`, an overflowing or out-of-bounds range, or a
/// guest without a memory.
pub fn write_guest_bytes(
    caller: &mut Caller<'_, HostState>,
    ptr: i64,
    bytes: &[u8],
) -> Result<(), i32> {
    let mem = guest_memory(caller)?;
    let len = i64::try_from(bytes.len()).map_err(|_| status::ERR_INVALID)?;
    match mem {
        GuestMem::Plain(m) => {
            let data = m.data_mut(&mut *caller);
            let r = range(data.len(), ptr, len)?;
            data[r].copy_from_slice(bytes);
            Ok(())
        }
        GuestMem::Shared(m) => {
            let r = range(m.data_size(), ptr, len)?;
            if crate::shared_mem::write(&m, r.start, bytes) {
                Ok(())
            } else {
                Err(status::ERR_INVALID)
            }
        }
    }
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
