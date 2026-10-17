//! The shared Wasmtime engine and its epoch ticker.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

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
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            tick: Duration::from_millis(10),
            pooling: None,
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
    _ticker: Arc<Ticker>,
}

impl OrbitEngine {
    /// Creates the engine: async execution, fuel metering, epoch interruption, Memory64.
    ///
    /// # Errors
    /// Fails if Wasmtime rejects the configuration.
    pub fn new(config: &EngineConfig) -> wasmtime::Result<Self> {
        let mut c = Config::new();
        c.consume_fuel(true);
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
            .finish_non_exhaustive()
    }
}
