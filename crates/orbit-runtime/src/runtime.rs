//! The [`Runtime`]: engine, registry, artifact cache and admission control.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Semaphore;

use crate::artifact::{Artifact, ContentHash};
use crate::capability::{CapabilityRegistry, Grants, InstanceId, TenantId};
use crate::engine::{EngineConfig, OrbitEngine};
use crate::error::{AdmissionError, InstantiateError, LoadError};
use crate::isolate::SpawnParams;
use crate::{Isolate, Limits};

/// Configuration of a [`Runtime`].
#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    /// Engine settings (tick, pooling allocator).
    pub engine: EngineConfig,
    /// Maximum number of invocations executing guest code at the same time, across all
    /// isolates. Further invocations wait (asynchronously) for a slot.
    pub max_concurrent_invocations: usize,
    /// Memory admission budget: the sum of `Limits::max_memory_bytes` of all live isolates
    /// may not exceed this. It reserves *limits*, not usage.
    pub memory_budget_bytes: u64,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            engine: EngineConfig::default(),
            max_concurrent_invocations: std::thread::available_parallelism()
                .map_or(4, std::num::NonZero::get)
                * 4,
            memory_budget_bytes: 4 * 1024 * 1024 * 1024,
        }
    }
}

#[derive(Debug)]
struct Budget {
    total: u64,
    reserved: Mutex<u64>,
}

/// RAII reservation of memory admission budget; released on drop.
#[derive(Debug)]
pub(crate) struct MemoryReservation {
    budget: Arc<Budget>,
    bytes: u64,
}

impl Drop for MemoryReservation {
    fn drop(&mut self) {
        let mut r = self
            .budget
            .reserved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *r = r.saturating_sub(self.bytes);
    }
}

struct Inner {
    engine: OrbitEngine,
    registry: CapabilityRegistry,
    cache: Mutex<HashMap<ContentHash, Arc<Artifact>>>,
    semaphore: Arc<Semaphore>,
    budget: Arc<Budget>,
    next_id: AtomicU64,
}

/// Owns the engine, the capabilities the host implements, the compiled-artifact cache and
/// admission control. Cheap to clone; clones share everything.
#[derive(Clone)]
pub struct Runtime {
    inner: Arc<Inner>,
}

impl Runtime {
    /// Creates a runtime serving the capabilities in `registry`.
    ///
    /// # Errors
    /// Fails if the engine cannot be created.
    pub fn new(config: RuntimeConfig, registry: CapabilityRegistry) -> wasmtime::Result<Self> {
        let engine = OrbitEngine::new(&config.engine)?;
        Ok(Self {
            inner: Arc::new(Inner {
                engine,
                registry,
                cache: Mutex::new(HashMap::new()),
                semaphore: Arc::new(Semaphore::new(config.max_concurrent_invocations.max(1))),
                budget: Arc::new(Budget {
                    total: config.memory_budget_bytes,
                    reserved: Mutex::new(0),
                }),
                next_id: AtomicU64::new(1),
            }),
        })
    }

    /// The shared engine.
    #[must_use]
    pub fn engine(&self) -> &OrbitEngine {
        &self.inner.engine
    }

    /// The capabilities this runtime serves.
    #[must_use]
    pub fn registry(&self) -> &CapabilityRegistry {
        &self.inner.registry
    }

    /// Validates and compiles `bytes` (binary or WAT), reusing the compiled module when the
    /// same content was loaded before (keyed by SHA-256 of the binary).
    ///
    /// # Errors
    /// A [`LoadError`]; failed loads are not cached.
    pub fn load(&self, bytes: &[u8]) -> Result<Arc<Artifact>, LoadError> {
        let wasm = crate::artifact::to_binary(bytes)?;
        let hash = crate::artifact::hash_of(&wasm);
        if let Some(a) = self.cache().get(&hash) {
            return Ok(Arc::clone(a));
        }
        let artifact = Arc::new(Artifact::load(
            &self.inner.engine,
            &self.inner.registry,
            &wasm,
        )?);
        Ok(Arc::clone(self.cache().entry(hash).or_insert(artifact)))
    }

    /// Number of distinct artifacts in the cache.
    #[must_use]
    pub fn cached_artifacts(&self) -> usize {
        self.cache().len()
    }

    /// Memory admission budget currently reserved by live isolates.
    #[must_use]
    pub fn reserved_memory_bytes(&self) -> u64 {
        *self
            .inner
            .budget
            .reserved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Invocation slots currently free.
    #[must_use]
    pub fn available_invocation_slots(&self) -> usize {
        self.inner.semaphore.available_permits()
    }

    /// Creates an isolate in [`IsolateState::Created`](crate::IsolateState::Created).
    ///
    /// Order of checks: every capability the artifact requires must be in `grants`
    /// ([`InstantiateError::NotGranted`]) and served by this host; then the memory budget
    /// is reserved ([`AdmissionError`]); then the module is instantiated under `limits`.
    /// Nothing guest-controlled runs before the grant check passes.
    ///
    /// # Errors
    /// An [`InstantiateError`].
    pub async fn spawn_isolate(
        &self,
        artifact: &Arc<Artifact>,
        tenant: impl Into<TenantId>,
        grants: &Grants,
        limits: Limits,
    ) -> Result<Isolate, InstantiateError> {
        for id in artifact.required() {
            if !grants.contains(id) {
                return Err(InstantiateError::NotGranted(id.clone()));
            }
        }
        for id in artifact.required() {
            if !self.inner.registry.contains(id) {
                return Err(InstantiateError::Unavailable(id.clone()));
            }
        }
        let reservation = self.reserve(limits.max_memory_bytes)?;
        let id = InstanceId(self.inner.next_id.fetch_add(1, Ordering::Relaxed));
        let params = SpawnParams {
            id,
            tenant: tenant.into(),
            limits,
            semaphore: Arc::clone(&self.inner.semaphore),
            reservation,
        };
        Isolate::create(artifact, grants, &self.inner.engine, params).await
    }

    fn reserve(&self, bytes: u64) -> Result<MemoryReservation, AdmissionError> {
        let budget = &self.inner.budget;
        let mut reserved = budget
            .reserved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let available = budget.total.saturating_sub(*reserved);
        if bytes > available {
            return Err(AdmissionError::MemoryBudget {
                requested: bytes,
                available,
            });
        }
        *reserved += bytes;
        Ok(MemoryReservation {
            budget: Arc::clone(budget),
            bytes,
        })
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, HashMap<ContentHash, Arc<Artifact>>> {
        self.inner
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field("capabilities", &self.inner.registry)
            .field("reserved_memory_bytes", &self.reserved_memory_bytes())
            .finish_non_exhaustive()
    }
}
