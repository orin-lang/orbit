//! The shared Wasmtime engine and its epoch ticker.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::error::EngineError;
use wasmtime::{Config, Engine, InstanceAllocationStrategy, PoolingAllocationConfig};

/// Pooling-allocator sizing. Slots are reserved up front (as virtual memory), which makes
/// instantiation cheaper at high isolate counts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolingConfig {
    /// Maximum live instances (and memories, and fibers) at once.
    pub max_instances: u32,
    /// Largest linear memory a slot can hold; isolates asking for more fail to instantiate.
    pub max_memory_bytes: usize,
}

impl Default for PoolingConfig {
    fn default() -> Self {
        Self {
            max_instances: 256,
            max_memory_bytes: 64 * 1024 * 1024,
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
            pc.total_tables(p.max_instances.saturating_mul(4));
            pc.total_stacks(p.max_instances);
            pc.max_memory_size(p.max_memory_bytes);
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
