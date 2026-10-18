//! The shared Wasmtime engine and its epoch ticker.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::error::EngineError;
use wasmtime::{Config, Engine, InstanceAllocationStrategy, PoolingAllocationConfig};

/// Pooling-allocator sizing and tuning.
///
/// The pooling allocator reserves one *slot* per possible instance up front (as virtual address
/// space) and reuses slots instead of mapping and unmapping memory for every instance, which
/// makes instantiation cheaper at high isolate counts and in the warm path (see
/// `docs/benchmarks.md`). Memory images of the module's data segments are mapped copy-on-write
/// (Wasmtime's `memory_init_cow`, on by default where the OS supports it), so a fresh instance
/// of a module with large data segments does not copy them.
///
/// The knobs that matter, and what they trade:
///
/// * `max_instances`: the hard cap on live isolates of the runtime, **including idle pooled
///   ones**. Virtual address space reserved is roughly
///   `max_instances * (max_memory_bytes + guard regions)`; it is not resident memory.
/// * `max_memory_bytes`: size of one linear-memory slot. An isolate whose
///   `Limits::max_memory_bytes` is larger cannot grow past the slot. Keep it near the largest
///   limit you actually grant.
/// * `max_table_elements` / `max_tables_per_instance`: slot size for tables (8 bytes an element).
/// * `*_keep_resident_bytes`: how much of a slot stays resident (is not returned to the OS with
///   `madvise`) when an instance is freed. `0` returns everything: lowest idle RSS, but the next
///   instance in that slot takes page faults again. A positive value makes slot reuse faster and
///   costs up to that many bytes of RSS per *free* slot. Use [`PoolingConfig::density`] for
///   many mostly-idle isolates and [`PoolingConfig::latency`] for hot, frequently recycled slots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolingConfig {
    /// Maximum live instances (and memories, and fibers) at once.
    pub max_instances: u32,
    /// Largest linear memory a slot can hold; isolates asking for more fail to instantiate.
    pub max_memory_bytes: usize,
    /// Maximum number of elements of any table (also bounds `Limits::max_table_elements`).
    pub max_table_elements: usize,
    /// Tables per instance the pool provides slots for (modules with more fail to instantiate).
    /// Compilers targeting WebAssembly emit one function table.
    pub max_tables_per_instance: u32,
    /// Bytes of each linear-memory slot kept resident after the instance is freed.
    pub memory_keep_resident_bytes: usize,
    /// Bytes of each table slot kept resident after the instance is freed.
    pub table_keep_resident_bytes: usize,
    /// Bytes of each fiber (async) stack kept resident after the instance is freed.
    pub stack_keep_resident_bytes: usize,
}

impl PoolingConfig {
    /// Preset for density: many mostly-idle isolates. Nothing stays resident in free slots.
    #[must_use]
    pub fn density(max_instances: u32) -> Self {
        Self {
            max_instances,
            ..Self::default()
        }
    }

    /// Preset for latency: few hot slots recycled quickly. Keeps 1 MiB of each memory slot,
    /// 64 KiB of each table and 64 KiB of each fiber stack resident, so a recycled slot
    /// avoids most page faults at the price of that much RSS per free slot.
    #[must_use]
    pub fn latency(max_instances: u32) -> Self {
        Self {
            max_instances,
            memory_keep_resident_bytes: 1024 * 1024,
            table_keep_resident_bytes: 64 * 1024,
            stack_keep_resident_bytes: 64 * 1024,
            ..Self::default()
        }
    }
}

impl Default for PoolingConfig {
    fn default() -> Self {
        Self {
            max_instances: 256,
            max_memory_bytes: 64 * 1024 * 1024,
            max_table_elements: 10_000,
            max_tables_per_instance: 1,
            memory_keep_resident_bytes: 0,
            table_keep_resident_bytes: 0,
            stack_keep_resident_bytes: 0,
        }
    }
}

/// Configuration of an [`OrbitEngine`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineConfig {
    /// Epoch tick. Deadlines and cooperative yields have this resolution.
    pub tick: Duration,
    /// Use the pooling instance allocator instead of the on-demand one.
    pub pooling: Option<PoolingConfig>,
    /// Compile fuel metering into guest code (default `false`).
    ///
    /// Fuel is an engine-wide, compile-time Wasmtime setting: metered code pays for the
    /// accounting on every run, even when an isolate's budget is unlimited, and costs roughly
    /// 1.2-1.8x on compute-heavy code (see `docs/benchmarks.md`, ADR-0008). With metering off,
    /// `Limits::fuel = Some(_)` is refused at spawn with
    /// [`InstantiateError::FuelMeteringDisabled`](crate::InstantiateError::FuelMeteringDisabled),
    /// fuel metrics are reported as "not measured" (`None`), and runaway code is bounded by
    /// `Limits::timeout` (epoch deadline) alone.
    pub fuel_metering: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            tick: Duration::from_millis(10),
            pooling: None,
            fuel_metering: false,
        }
    }
}

struct Ticker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            h.thread().unpark();
            let _ = h.join();
        }
    }
}

/// One shared `wasmtime::Engine` configured for isolates, plus a background thread that
/// advances its epoch every `tick`. Cheap to clone; the ticker stops when the last clone
/// is dropped.
#[derive(Clone)]
pub struct OrbitEngine {
    engine: Engine,
    tick: Duration,
    fuel_metering: bool,
    _ticker: Arc<Ticker>,
}

impl OrbitEngine {
    /// Creates the engine: async execution, epoch interruption, optional fuel metering ([`EngineConfig::fuel_metering`]), Memory64.
    ///
    /// # Errors
    /// Fails if Wasmtime rejects the configuration.
    pub fn new(config: &EngineConfig) -> Result<Self, EngineError> {
        Self::build(config).map_err(|e| EngineError(format!("{e:#}")))
    }

    fn build(config: &EngineConfig) -> wasmtime::Result<Self> {
        let mut c = Config::new();
        c.consume_fuel(config.fuel_metering);
        c.epoch_interruption(true);
        c.wasm_memory64(true);
        if let Some(p) = &config.pooling {
            let mut pc = PoolingAllocationConfig::default();
            pc.total_core_instances(p.max_instances);
            pc.total_memories(p.max_instances);
            pc.total_tables(p.max_instances.saturating_mul(p.max_tables_per_instance));
            pc.max_tables_per_module(p.max_tables_per_instance);
            pc.total_stacks(p.max_instances);
            pc.max_memory_size(p.max_memory_bytes);
            pc.table_elements(p.max_table_elements);
            pc.linear_memory_keep_resident(p.memory_keep_resident_bytes);
            pc.table_keep_resident(p.table_keep_resident_bytes);
            pc.async_stack_keep_resident(p.stack_keep_resident_bytes);
            c.allocation_strategy(InstanceAllocationStrategy::Pooling(pc));
        }
        let engine = Engine::new(&c)?;

        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let engine = engine.clone();
            let stop = Arc::clone(&stop);
            let tick = config.tick;
            thread::Builder::new()
                .name("orbit-epoch".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        thread::park_timeout(tick);
                        engine.increment_epoch();
                    }
                })?
        };
        Ok(Self {
            engine,
            tick: config.tick,
            fuel_metering: config.fuel_metering,
            _ticker: Arc::new(Ticker {
                stop,
                handle: Some(handle),
            }),
        })
    }

    /// The underlying Wasmtime engine.
    #[must_use]
    pub fn wasmtime(&self) -> &Engine {
        &self.engine
    }

    /// Whether code compiled by this engine is fuel-metered.
    #[must_use]
    pub fn fuel_metering(&self) -> bool {
        self.fuel_metering
    }

    /// The epoch tick.
    #[must_use]
    pub fn tick(&self) -> Duration {
        self.tick
    }
}

impl std::fmt::Debug for OrbitEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrbitEngine")
            .field("tick", &self.tick)
            .field("fuel_metering", &self.fuel_metering)
            .finish_non_exhaustive()
    }
}
