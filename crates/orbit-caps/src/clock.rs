//! `orbit:clock/v1`: `now_unix_nanos() -> i64`, `monotonic_nanos() -> i64`.
//!
//! Both readings are coarsened to a configurable granularity (default 1 ms) to limit timing
//! side channels. Grant config: `{"resolution_ns": 1000000}`.

use std::any::Any;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use orbit_abi::CapabilityId;
use orbit_runtime::{Capability, HostState, InstanceInfo};
use wasmtime::{Caller, Linker};

/// Default coarsening granularity: 1 ms.
pub const DEFAULT_RESOLUTION_NS: u64 = 1_000_000;

/// Where time comes from. Swap it for a [`ManualClock`] in deterministic tests.
pub trait TimeSource: Send + Sync + 'static {
    /// Nanoseconds since the Unix epoch (wall clock; may jump).
    fn now_unix_nanos(&self) -> i64;
    /// Nanoseconds on a monotonic clock with an arbitrary origin.
    fn monotonic_nanos(&self) -> u64;
}

/// The operating system's clocks.
#[derive(Debug)]
pub struct SystemTimeSource {
    origin: Instant,
}

impl SystemTimeSource {
    /// A system time source whose monotonic origin is now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemTimeSource {
    fn default() -> Self {
        Self::new()
    }
}

impl TimeSource for SystemTimeSource {
    fn now_unix_nanos(&self) -> i64 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
            Err(e) => -i64::try_from(e.duration().as_nanos()).unwrap_or(i64::MAX),
        }
    }

    fn monotonic_nanos(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}

/// A settable clock for tests. Clones share the same time; the monotonic reading advances
/// together with the wall reading.
#[derive(Clone, Debug, Default)]
pub struct ManualClock {
    unix: Arc<AtomicI64>,
    mono: Arc<AtomicI64>,
}

impl ManualClock {
    /// A clock at wall time `unix_nanos` and monotonic time 0.
    #[must_use]
    pub fn new(unix_nanos: i64) -> Self {
        Self {
            unix: Arc::new(AtomicI64::new(unix_nanos)),
            mono: Arc::new(AtomicI64::new(0)),
        }
    }

    /// Moves both clocks forward.
    pub fn advance(&self, nanos: u64) {
        let n = i64::try_from(nanos).unwrap_or(i64::MAX);
        self.unix.fetch_add(n, Ordering::SeqCst);
        self.mono.fetch_add(n, Ordering::SeqCst);
    }

    /// Sets the wall clock only (it may go backwards, as a real one can).
    pub fn set_unix_nanos(&self, nanos: i64) {
        self.unix.store(nanos, Ordering::SeqCst);
    }
}

impl TimeSource for ManualClock {
    fn now_unix_nanos(&self) -> i64 {
        self.unix.load(Ordering::SeqCst)
    }

    fn monotonic_nanos(&self) -> u64 {
        u64::try_from(self.mono.load(Ordering::SeqCst)).unwrap_or(0)
    }
}

#[derive(Debug)]
struct ClockState {
    /// Monotonic reading of the source when the instance was created.
    base: u64,
    resolution_ns: u64,
}

/// The `orbit:clock/v1` capability.
#[derive(Clone)]
pub struct ClockCapability {
    source: Arc<dyn TimeSource>,
}

impl ClockCapability {
    /// A clock capability using the operating system's time.
    #[must_use]
    pub fn system() -> Self {
        Self::new(SystemTimeSource::new())
    }

    /// The time source this capability reads. `wasi_p1/v1` shares it so that WASI clocks
    /// and `orbit:clock/v1` agree (and a test's [`ManualClock`] drives both).
    #[must_use]
    pub fn time_source(&self) -> Arc<dyn TimeSource> {
        Arc::clone(&self.source)
    }

    /// A clock capability reading `source`.
    #[must_use]
    pub fn new(source: impl TimeSource) -> Self {
        Self {
            source: Arc::new(source),
        }
    }
}

impl fmt::Debug for ClockCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClockCapability").finish_non_exhaustive()
    }
}

pub(crate) fn parse_resolution(config: &serde_json::Value) -> u64 {
    config
        .get("resolution_ns")
        .and_then(serde_json::Value::as_u64)
        .map_or(DEFAULT_RESOLUTION_NS, |r| r.max(1))
}

pub(crate) fn coarsen_u(v: u64, res: u64) -> u64 {
    v - v % res
}

pub(crate) fn coarsen_i(v: i64, res: u64) -> i64 {
    let res = i64::try_from(res).unwrap_or(i64::MAX);
    v - v.rem_euclid(res)
}

impl Capability for ClockCapability {
    fn id(&self) -> CapabilityId {
        CapabilityId::clock_v1()
    }

    fn link(&self, linker: &mut Linker<HostState>) -> wasmtime::Result<()> {
        let id = self.id();
        let module = id.import_module();
        let source = Arc::clone(&self.source);
        let (id2, source2) = (id.clone(), Arc::clone(&source));
        linker.func_wrap(
            &module,
            "now_unix_nanos",
            move |mut caller: Caller<'_, HostState>| -> i64 {
                caller.data_mut().record_call(&id2);
                let res = caller
                    .data_mut()
                    .cap_state_mut::<ClockState>(&id2)
                    .map_or(DEFAULT_RESOLUTION_NS, |s| s.resolution_ns);
                coarsen_i(source2.now_unix_nanos(), res)
            },
        )?;
        linker.func_wrap(
            &module,
            "monotonic_nanos",
            move |mut caller: Caller<'_, HostState>| -> i64 {
                caller.data_mut().record_call(&id);
                let (base, res) = caller
                    .data_mut()
                    .cap_state_mut::<ClockState>(&id)
                    .map_or((0, DEFAULT_RESOLUTION_NS), |s| (s.base, s.resolution_ns));
                let elapsed = source.monotonic_nanos().saturating_sub(base);
                i64::try_from(coarsen_u(elapsed, res)).unwrap_or(i64::MAX)
            },
        )?;
        Ok(())
    }

    fn instance_state(
        &self,
        _info: &InstanceInfo,
        config: &serde_json::Value,
    ) -> Box<dyn Any + Send> {
        Box::new(ClockState {
            base: self.source.monotonic_nanos(),
            resolution_ns: parse_resolution(config),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coarsening() {
        assert_eq!(coarsen_u(1_234_567, 1_000_000), 1_000_000);
        assert_eq!(coarsen_i(-1, 1_000_000), -1_000_000);
        assert_eq!(coarsen_i(2_500_000, 1), 2_500_000);
        assert_eq!(parse_resolution(&serde_json::Value::Null), 1_000_000);
        assert_eq!(
            parse_resolution(&serde_json::json!({"resolution_ns": 0})),
            1
        );
        assert_eq!(
            parse_resolution(&serde_json::json!({"resolution_ns": 50})),
            50
        );
    }
}
