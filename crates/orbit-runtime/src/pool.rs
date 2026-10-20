//! Instance pools: pre-instantiated, already-initialised idle isolates for warm starts.
//! See [`InstancePool`] for the semantics, the reuse policies and their security trade-off.

use std::collections::VecDeque;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::artifact::Artifact;
use crate::capability::{Grants, TenantId};
use crate::error::{ExecError, InstantiateError};
use crate::{Isolate, IsolateState, Limits, Runtime};

/// What happens to an isolate when its caller is done with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReusePolicy {
    /// Every acquire gets a never-used isolate; used ones are terminated on release. The
    /// default: no state can leak between requests. See the [`InstancePool`] docs.
    FreshPerAcquire,
    /// A healthy isolate returns to the pool and is handed out again, until it has been handed
    /// out `max_uses` times (a value below 1 counts as 1). **Guest state persists across
    /// requests of the same tenant.** See the [`InstancePool`] docs.
    Reuse {
        /// Number of acquires one isolate serves before it is terminated.
        max_uses: u64,
    },
}

/// Sizing and policy of an [`InstancePool`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolConfig {
    /// Idle isolates the background task keeps ready (best effort: it stops while the memory
    /// budget or `max_total` leaves no room, and idle isolates evicted by
    /// [`Runtime::reclaim_idle`] are not refilled until the next acquire or release). `0` lets
    /// the pool scale to zero.
    pub min_idle: usize,
    /// Upper bound on idle isolates; released isolates beyond it are terminated.
    pub max_idle: usize,
    /// Upper bound on isolates of this pool (idle plus checked out plus being created).
    /// [`InstancePool::acquire`] waits when it is reached.
    pub max_total: usize,
    /// See [`ReusePolicy`].
    pub reuse: ReusePolicy,
    /// Idle isolates older than this are terminated by the background task, down to
    /// `min_idle`. `None` keeps them until evicted under memory pressure or the pool is dropped.
    pub idle_timeout: Option<Duration>,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            min_idle: 0,
            max_idle: 8,
            max_total: 64,
            reuse: ReusePolicy::FreshPerAcquire,
            idle_timeout: Some(Duration::from_secs(60)),
        }
    }
}

impl PoolConfig {
    /// Sets [`PoolConfig::min_idle`].
    #[must_use]
    pub fn with_min_idle(mut self, n: usize) -> Self {
        self.min_idle = n;
        self
    }

    /// Sets [`PoolConfig::max_idle`].
    #[must_use]
    pub fn with_max_idle(mut self, n: usize) -> Self {
        self.max_idle = n;
        self
    }

    /// Sets [`PoolConfig::max_total`].
    #[must_use]
    pub fn with_max_total(mut self, n: usize) -> Self {
        self.max_total = n;
        self
    }

    /// Sets [`PoolConfig::reuse`].
    #[must_use]
    pub fn with_reuse(mut self, reuse: ReusePolicy) -> Self {
        self.reuse = reuse;
        self
    }

    /// Sets [`PoolConfig::idle_timeout`].
    #[must_use]
    pub fn with_idle_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.idle_timeout = timeout;
        self
    }

    /// Makes the bounds consistent: `max_total >= 1`, `min_idle <= max_total`,
    /// `max_idle >= min_idle`, and `max_idle <= max_total`.
    fn normalised(mut self) -> Self {
        self.max_total = self.max_total.max(1);
        self.min_idle = self.min_idle.min(self.max_total);
        self.max_idle = self.max_idle.max(self.min_idle).min(self.max_total);
        if let ReusePolicy::Reuse { max_uses } = &mut self.reuse {
            *max_uses = (*max_uses).max(1);
        }
        self
    }
}

/// Why a pool operation failed.
#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    /// Creating an isolate failed (grants, admission, instantiation).
    #[error(transparent)]
    Instantiate(#[from] InstantiateError),
    /// A new isolate failed its initialisation (`_initialize` / `orbit_init`).
    #[error("initialisation of a pooled isolate failed: {0}")]
    Init(ExecError),
    /// The pool was shut down.
    #[error("instance pool is closed")]
    Closed,
}

/// A snapshot of a pool's counters and occupancy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PoolStats {
    /// Acquires served from an idle isolate (exact).
    pub hits: u64,
    /// Acquires that had to create an isolate (exact). The caller waited for its creation.
    pub misses: u64,
    /// Isolates created over the pool's life: misses, `prewarm` and background refills.
    pub created: u64,
    /// Isolates returned to idle for reuse ([`ReusePolicy::Reuse`] only).
    pub recycled: u64,
    /// Isolates terminated on release: used under `FreshPerAcquire`, not healthy, over
    /// `max_uses`, over `max_idle`, discarded explicitly, or released after the pool closed.
    pub discarded: u64,
    /// Idle isolates terminated by idle timeout, memory reclaim or pool shutdown.
    pub evicted: u64,
    /// Attempts to create an isolate that failed (admission, instantiation, init).
    pub create_failures: u64,
    /// Idle isolates now.
    pub idle: usize,
    /// Isolates handed out and not yet released (including ones being created for a caller).
    pub in_use: usize,
    /// Bytes of the admission budget held by the idle isolates (`idle * max_memory_bytes`).
    pub idle_reserved_bytes: u64,
    /// Bytes of the admission budget held by the whole pool (`total * max_memory_bytes`).
    pub reserved_bytes: u64,
}

#[derive(Default)]
struct Counters {
    hits: AtomicU64,
    misses: AtomicU64,
    created: AtomicU64,
    recycled: AtomicU64,
    discarded: AtomicU64,
    evicted: AtomicU64,
    create_failures: AtomicU64,
}

fn bump(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

struct Idle {
    iso: Isolate,
    uses: u64,
    /// When it became idle (created or released); the LRU key.
    since: Instant,
}

#[derive(Default)]
struct State {
    /// Oldest at the front, most recently idle at the back. Acquire takes from the back.
    idle: VecDeque<Idle>,
    /// Idle + checked out + being created. Slots are accounted before the creation starts.
    total: usize,
    /// Slots being created that will land in `idle` (prewarm and refill).
    pending_idle: usize,
    closed: bool,
}

/// State shared by the pool handle, the guards and the background task.
pub(crate) struct PoolShared {
    rt: Runtime,
    artifact: Arc<Artifact>,
    tenant: TenantId,
    grants: Grants,
    limits: Limits,
    cfg: PoolConfig,
    state: Mutex<State>,
    /// Wakes `acquire` calls waiting for an isolate or a free slot.
    available: Notify,
    /// Wakes the background task.
    refill: Arc<Notify>,
    counters: Counters,
}

/// A slot reserved in `State::total` that is released again unless defused.
struct Slot {
    shared: Arc<PoolShared>,
    for_idle: bool,
    armed: bool,
}

impl Slot {
    /// The slot becomes the caller's isolate.
    fn defuse(mut self) {
        self.armed = false;
    }

    /// The slot becomes an idle isolate.
    fn into_idle(mut self, iso: Isolate) {
        self.armed = false;
        let doomed = {
            let mut st = self.shared.lock();
            st.pending_idle -= 1;
            if st.closed {
                st.total -= 1;
                Some(iso)
            } else {
                st.idle.push_back(Idle {
                    iso,
                    uses: 0,
                    since: Instant::now(),
                });
                None
            }
        };
        if let Some(mut iso) = doomed {
            iso.terminate();
            self.shared.available.notify_one();
        } else {
            self.shared.available.notify_one();
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        {
            let mut st = self.shared.lock();
            st.total -= 1;
            if self.for_idle {
                st.pending_idle -= 1;
            }
        }
        self.shared.available.notify_one();
    }
}

impl PoolShared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn max_memory(&self) -> u64 {
        self.limits.max_memory_bytes
    }

    /// Spawns and initialises one isolate. `reclaim`: may evict other pools' idle isolates.
    async fn create_isolate(&self, reclaim: bool) -> Result<Isolate, PoolError> {
        let made = self
            .rt
            .spawn_inner(
                &self.artifact,
                self.tenant.clone(),
                &self.grants,
                self.limits.clone(),
                reclaim,
            )
            .await;
        let mut iso = match made {
            Ok(i) => i,
            Err(e) => {
                bump(&self.counters.create_failures);
                return Err(PoolError::Instantiate(e));
            }
        };
        if let Err(e) = iso.init().await {
            iso.terminate();
            bump(&self.counters.create_failures);
            return Err(PoolError::Init(e));
        }
        bump(&self.counters.created);
        Ok(iso)
    }

    /// Reserves a slot for an idle isolate if fewer than `target` idle ones exist or are
    /// being made and the pool has room.
    fn reserve_idle_slot(self: &Arc<Self>, target: usize) -> Option<Slot> {
        let mut st = self.lock();
        if st.closed || st.idle.len() + st.pending_idle >= target || st.total >= self.cfg.max_total
        {
            return None;
        }
        st.total += 1;
        st.pending_idle += 1;
        Some(Slot {
            shared: Arc::clone(self),
            for_idle: true,
            armed: true,
        })
    }

    async fn acquire_inner(
        self: &Arc<Self>,
        wait: bool,
    ) -> Result<Option<PooledIsolate>, PoolError> {
        #[allow(clippy::large_enum_variant)] // short-lived local, never stored
        enum Next {
            Hit(Idle),
            Create(Slot),
            Wait,
        }
        loop {
            let next = {
                let mut st = self.lock();
                if st.closed {
                    return Err(PoolError::Closed);
                }
                if let Some(idle) = st.idle.pop_back() {
                    Next::Hit(idle)
                } else if st.total < self.cfg.max_total {
                    st.total += 1;
                    Next::Create(Slot {
                        shared: Arc::clone(self),
                        for_idle: false,
                        armed: true,
                    })
                } else {
                    Next::Wait
                }
            };
            match next {
                Next::Hit(idle) => {
                    bump(&self.counters.hits);
                    self.refill.notify_one();
                    return Ok(Some(self.guard(idle.iso, idle.uses + 1)));
                }
                Next::Create(slot) => {
                    bump(&self.counters.misses);
                    // If this future is dropped here the slot guard gives the slot back.
                    let iso = self.create_isolate(true).await?;
                    slot.defuse();
                    return Ok(Some(self.guard(iso, 1)));
                }
                Next::Wait if !wait => return Ok(None),
                Next::Wait => self.available.notified().await,
            }
        }
    }

    fn guard(self: &Arc<Self>, iso: Isolate, uses: u64) -> PooledIsolate {
        PooledIsolate {
            shared: Arc::clone(self),
            entry: Some((iso, uses)),
            discard: false,
        }
    }

    /// Takes an isolate back from a guard.
    fn release(&self, iso: Isolate, uses: u64, discard: bool) {
        let healthy = matches!(iso.state(), IsolateState::Idle | IsolateState::Initialized);
        let reusable = match self.cfg.reuse {
            ReusePolicy::FreshPerAcquire => false,
            ReusePolicy::Reuse { max_uses } => !discard && healthy && uses < max_uses,
        };
        let leftover = if reusable {
            let mut st = self.lock();
            if !st.closed && st.idle.len() + st.pending_idle < self.cfg.max_idle {
                st.idle.push_back(Idle {
                    iso,
                    uses,
                    since: Instant::now(),
                });
                None
            } else {
                Some(iso)
            }
        } else {
            Some(iso)
        };
        match leftover {
            None => {
                bump(&self.counters.recycled);
                self.available.notify_one();
                self.refill.notify_one();
            }
            Some(mut iso) => {
                // Terminate first so the memory reservation is back before a waiter retries.
                iso.terminate();
                bump(&self.counters.discarded);
                self.lock().total -= 1;
                self.available.notify_one();
                self.refill.notify_one();
            }
        }
    }

    /// One round of background work: evict expired idle isolates, refill to `min_idle`.
    /// Returns how long to sleep before the next round (`None`: until signalled).
    async fn maintain(self: &Arc<Self>) -> Option<Duration> {
        let mut doomed = Vec::new();
        if let Some(timeout) = self.cfg.idle_timeout {
            let now = Instant::now();
            let mut st = self.lock();
            while st.idle.len() > self.cfg.min_idle
                && st
                    .idle
                    .front()
                    .is_some_and(|f| now.saturating_duration_since(f.since) >= timeout)
            {
                if let Some(f) = st.idle.pop_front() {
                    st.total -= 1;
                    doomed.push(f.iso);
                }
            }
        }
        for mut iso in doomed {
            iso.terminate();
            bump(&self.counters.evicted);
            self.available.notify_one();
        }

        let mut retry = None;
        while let Some(slot) = self.reserve_idle_slot(self.cfg.min_idle) {
            match self.create_isolate(false).await {
                Ok(iso) => slot.into_idle(iso),
                Err(_) => {
                    drop(slot);
                    retry = Some(Duration::from_secs(1));
                    break;
                }
            }
        }
        if retry.is_some() {
            return retry;
        }
        let timeout = self.cfg.idle_timeout?;
        let st = self.lock();
        if st.idle.len() <= self.cfg.min_idle {
            return None;
        }
        let front = st.idle.front()?;
        let age = Instant::now().saturating_duration_since(front.since);
        Some(timeout.saturating_sub(age).max(Duration::from_millis(1)))
    }

    /// `since` of the longest-idle isolate, the LRU key for [`Runtime::reclaim_idle`].
    pub(crate) fn oldest_idle(&self) -> Option<Instant> {
        self.lock().idle.front().map(|f| f.since)
    }

    /// Terminates the longest-idle isolate; returns the memory it held.
    pub(crate) fn evict_oldest(&self) -> Option<u64> {
        let mut iso = {
            let mut st = self.lock();
            let f = st.idle.pop_front()?;
            st.total -= 1;
            f.iso
        };
        iso.terminate();
        bump(&self.counters.evicted);
        // Not signalling the refill task: it would race the caller that reclaimed for the
        // freed memory.
        self.available.notify_one();
        Some(self.max_memory())
    }

    fn close(&self) {
        let doomed: Vec<Idle> = {
            let mut st = self.lock();
            st.closed = true;
            let doomed: Vec<Idle> = st.idle.drain(..).collect();
            st.total -= doomed.len();
            doomed
        };
        for mut d in doomed {
            d.iso.terminate();
            bump(&self.counters.evicted);
        }
        self.available.notify_waiters();
        // Waiters re-check `closed`; wake them all.
        self.available.notify_one();
    }

    fn stats(&self) -> PoolStats {
        let (idle, total) = {
            let st = self.lock();
            (st.idle.len(), st.total)
        };
        let c = &self.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        PoolStats {
            hits: get(&c.hits),
            misses: get(&c.misses),
            created: get(&c.created),
            recycled: get(&c.recycled),
            discarded: get(&c.discarded),
            evicted: get(&c.evicted),
            create_failures: get(&c.create_failures),
            idle,
            in_use: total.saturating_sub(idle),
            idle_reserved_bytes: idle as u64 * self.max_memory(),
            reserved_bytes: total as u64 * self.max_memory(),
        }
    }
}

async fn background(shared: std::sync::Weak<PoolShared>, signal: Arc<Notify>) {
    loop {
        let Some(pool) = shared.upgrade() else { return };
        let wake = pool.maintain().await;
        drop(pool);
        match wake {
            Some(d) => {
                tokio::select! {
                    () = signal.notified() => {}
                    () = tokio::time::sleep(d) => {}
                }
            }
            None => signal.notified().await,
        }
    }
}

struct PoolHandle {
    shared: Arc<PoolShared>,
    task: JoinHandle<()>,
}

impl Drop for PoolHandle {
    fn drop(&mut self) {
        self.task.abort();
        self.shared.close();
    }
}

/// A pool of ready isolates for one artifact, tenant, grant set and limits.
///
/// Cheap to clone; clones share the pool. When the last clone is dropped the background task
/// stops and idle isolates are terminated.
///
/// An [`InstancePool`] belongs to exactly one *(artifact, tenant, grants, limits)* tuple that is
/// fixed when [`Runtime::pool`] creates it. [`InstancePool::acquire`] has no parameter that could
/// select another tenant or another grant set, and a [`PooledIsolate`] can only be returned to
/// the pool it came from, so isolates of one tenant can never be handed to another tenant, and
/// an isolate created with one set of grants never runs under a different one. Two calls to
/// [`Runtime::pool`] always make two independent pools; the runtime does not merge them.
///
/// # Reuse policy and its security trade-off
///
/// * [`ReusePolicy::FreshPerAcquire`] (the default). Every acquire hands out an instance that
///   has never been used by a caller; when it is released it is terminated (store, memory and
///   handles dropped, memory reservation returned). Nothing a request does, in guest memory,
///   globals, tables or capability handles, is visible to the next request. The pool only
///   saves the *instantiation and initialisation* latency by doing it ahead of time.
/// * [`ReusePolicy::Reuse`]. An instance that is still [`Idle`](crate::IsolateState::Idle) (or
///   untouched `Initialized`) returns to the pool after the caller is done, at most `max_uses`
///   times. The next acquire **sees everything the previous request left behind**: linear
///   memory, mutable globals, tables, open capability handles (for example KV namespace
///   handles), and anything the guest cached. Only the per-invocation budgets are reset (fuel,
///   deadline, log budget). The state never crosses tenants or grant sets (see above), but it
///   does cross *requests of the same tenant*, so use it only for guests that are written for
///   it (stateless handlers or deliberate in-memory caches) and never when two requests of the
///   same tenant must not influence each other (for example requests on behalf of different
///   end users whose data the guest might keep). `max_uses` bounds how long leaked state or a
///   slowly growing heap can live.
///
/// In both modes an isolate that is `Failed` (trap, fuel, deadline, memory limit, `proc_exit`),
/// `Terminated`, or still `Running` (the invocation future was cancelled mid-call) is never
/// returned to the pool; it is terminated.
///
/// # Memory accounting
///
/// A pooled idle isolate holds its [`Limits::max_memory_bytes`] reservation in the runtime's
/// admission budget exactly like a running one: that is the honest cost of keeping it warm.
/// [`PoolStats::idle_reserved_bytes`] reports it. When a spawn fails admission,
/// [`Runtime::reclaim_idle`] terminates idle pooled isolates, least recently used first, until
/// the reservation fits ([`Runtime::spawn_isolate`] does this automatically).
///
/// # Background task
///
/// Each pool runs one Tokio task that keeps `min_idle` isolates ready (refilling after
/// acquires and discards) and, when `idle_timeout` is set, terminates isolates that sat idle for
/// longer, down to `min_idle`. With `min_idle = 0` an unused pool therefore scales to zero. The
/// task holds only a weak reference and is aborted when the last [`InstancePool`] handle is
/// dropped; dropping the handle also terminates the idle isolates (checked-out ones are
/// terminated when released).
///
/// # Example
///
/// ```ignore
/// let pool = rt.pool(&artifact, "tenant-a", &grants, limits, PoolConfig::default().with_min_idle(2))?;
/// let mut iso = pool.acquire().await?;          // never used before (FreshPerAcquire)
/// let out = iso.invoke("handle", &[1]).await?;
/// drop(iso);                                    // terminated; the pool refills in the background
/// ```
#[derive(Clone)]
pub struct InstancePool {
    handle: Arc<PoolHandle>,
}

impl InstancePool {
    fn shared(&self) -> &Arc<PoolShared> {
        &self.handle.shared
    }

    /// Hands out an isolate in state `Initialized` (fresh) or `Idle` (reused), waiting if
    /// `max_total` isolates are checked out. Cancel-safe: dropping the future loses nothing.
    /// Wrap it in `tokio::time::timeout` to bound the wait.
    ///
    /// # Errors
    /// [`PoolError`]: creating an isolate failed (for example memory admission), or the pool
    /// was closed.
    pub async fn acquire(&self) -> Result<PooledIsolate, PoolError> {
        match self.shared().acquire_inner(true).await? {
            Some(g) => Ok(g),
            None => Err(PoolError::Closed), // unreachable: waiting never yields `None`
        }
    }

    /// Like [`acquire`](Self::acquire) but returns `Ok(None)` instead of waiting when
    /// `max_total` isolates are checked out.
    ///
    /// # Errors
    /// As for [`acquire`](Self::acquire).
    pub async fn try_acquire(&self) -> Result<Option<PooledIsolate>, PoolError> {
        self.shared().acquire_inner(false).await
    }

    /// Creates idle isolates until `n` are idle (at most `max_idle`, and as far as `max_total`
    /// allows). Returns how many it created.
    ///
    /// # Errors
    /// The first creation failure (isolates created before it stay in the pool).
    pub async fn prewarm(&self, n: usize) -> Result<usize, PoolError> {
        let shared = self.shared();
        let target = n.min(shared.cfg.max_idle);
        let mut made = 0;
        while let Some(slot) = shared.reserve_idle_slot(target) {
            let iso = shared.create_isolate(true).await?;
            slot.into_idle(iso);
            made += 1;
        }
        Ok(made)
    }

    /// A snapshot of the counters and occupancy.
    #[must_use]
    pub fn stats(&self) -> PoolStats {
        self.shared().stats()
    }

    /// The effective configuration (bounds made consistent: `max_total >= 1`,
    /// `min_idle <= max_total`, `max_idle` between `min_idle` and `max_total`).
    #[must_use]
    pub fn config(&self) -> &PoolConfig {
        &self.shared().cfg
    }

    /// The tenant every isolate of this pool belongs to.
    #[must_use]
    pub fn tenant(&self) -> &TenantId {
        &self.shared().tenant
    }

    /// The grants every isolate of this pool was created with.
    #[must_use]
    pub fn grants(&self) -> &Grants {
        &self.shared().grants
    }

    /// The limits every isolate of this pool was created with.
    #[must_use]
    pub fn limits(&self) -> &Limits {
        &self.shared().limits
    }

    /// The artifact this pool instantiates.
    #[must_use]
    pub fn artifact(&self) -> &Arc<Artifact> {
        &self.shared().artifact
    }

    /// Shuts the pool down: terminates idle isolates, makes `acquire` fail with
    /// [`PoolError::Closed`] and terminates checked-out isolates when they are released.
    pub fn close(&self) {
        self.shared().close();
    }
}

impl std::fmt::Debug for InstancePool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstancePool")
            .field("tenant", &self.shared().tenant)
            .field("config", &self.shared().cfg)
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

/// An isolate checked out of an [`InstancePool`]. Derefs to [`Isolate`].
///
/// Dropping the guard (or [`release`](Self::release)) returns the isolate: terminated under
/// [`ReusePolicy::FreshPerAcquire`], back to idle under [`ReusePolicy::Reuse`] when it is
/// healthy and under `max_uses`.
pub struct PooledIsolate {
    shared: Arc<PoolShared>,
    entry: Option<(Isolate, u64)>,
    discard: bool,
}

impl PooledIsolate {
    /// How many times this isolate has been handed out, including now (1: first use).
    #[must_use]
    pub fn uses(&self) -> u64 {
        self.entry.as_ref().map_or(0, |e| e.1)
    }

    /// Returns the isolate to its pool (same as dropping the guard).
    pub fn release(self) {
        drop(self);
    }

    /// Terminates the isolate instead of returning it, whatever the reuse policy.
    pub fn discard(mut self) {
        self.discard = true;
    }
}

impl Deref for PooledIsolate {
    type Target = Isolate;
    fn deref(&self) -> &Isolate {
        &self.entry.as_ref().expect("guard holds an isolate").0
    }
}

impl DerefMut for PooledIsolate {
    fn deref_mut(&mut self) -> &mut Isolate {
        &mut self.entry.as_mut().expect("guard holds an isolate").0
    }
}

impl Drop for PooledIsolate {
    fn drop(&mut self) {
        if let Some((iso, uses)) = self.entry.take() {
            self.shared.release(iso, uses, self.discard);
        }
    }
}

impl std::fmt::Debug for PooledIsolate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PooledIsolate")
            .field("isolate", &self.entry.as_ref().map(|e| &e.0))
            .field("uses", &self.uses())
            .finish()
    }
}

impl Runtime {
    /// Creates an [`InstancePool`] of isolates of `artifact` for `tenant` with `grants` and
    /// `limits`, and starts its background task (which fills the pool to `config.min_idle`).
    ///
    /// The tuple is fixed for the life of the pool; there is no way to acquire an isolate for
    /// another tenant or grant set from it. The runtime keeps the pool in a registry (weakly)
    /// so [`reclaim_idle`](Self::reclaim_idle) can evict from it. Keep the returned handle (it
    /// is a cheap clone) for as long as the application should stay warm.
    ///
    /// # Errors
    /// The same grant checks as [`Runtime::spawn_isolate`] ([`InstantiateError::NotGranted`],
    /// [`InstantiateError::Unavailable`]) and [`InstantiateError::FuelMeteringDisabled`] when
    /// `limits.fuel` is set on an unmetered engine, so a misconfigured pool fails here and not
    /// on the first request.
    ///
    /// # Panics
    /// Outside a Tokio runtime (the background task is spawned with `tokio::spawn`).
    pub fn pool(
        &self,
        artifact: &Arc<Artifact>,
        tenant: impl Into<TenantId>,
        grants: &Grants,
        limits: Limits,
        config: PoolConfig,
    ) -> Result<InstancePool, InstantiateError> {
        self.check_grants(artifact, grants)?;
        if artifact.shared_memory().is_some() && matches!(config.reuse, ReusePolicy::Reuse { .. }) {
            return Err(InstantiateError::ThreadsCannotBeReused);
        }
        if limits.fuel.is_some() && !self.engine().fuel_metering() {
            return Err(InstantiateError::FuelMeteringDisabled);
        }
        let refill = Arc::new(Notify::new());
        let shared = Arc::new(PoolShared {
            rt: self.clone(),
            artifact: Arc::clone(artifact),
            tenant: tenant.into(),
            grants: grants.clone(),
            limits,
            cfg: config.normalised(),
            state: Mutex::new(State::default()),
            available: Notify::new(),
            refill: Arc::clone(&refill),
            counters: Counters::default(),
        });
        self.register_pool(Arc::downgrade(&shared));
        let task = tokio::spawn(background(Arc::downgrade(&shared), refill));
        Ok(InstancePool {
            handle: Arc::new(PoolHandle { shared, task }),
        })
    }

    /// Terminates idle pooled isolates, least recently used first (across all pools of this
    /// runtime), until at least `bytes_needed` of the memory admission budget is unreserved or
    /// no idle isolate is left. Returns the bytes released by this call.
    ///
    /// [`Runtime::spawn_isolate`] calls this when a reservation fails, so applications rarely
    /// need to. Isolates checked out of a pool are never touched.
    pub fn reclaim_idle(&self, bytes_needed: u64) -> u64 {
        let mut freed = 0;
        while self.free_memory_bytes() < bytes_needed {
            let pools = self.live_pools();
            let victim = pools
                .iter()
                .filter_map(|p| p.oldest_idle().map(|t| (t, p)))
                .min_by_key(|(t, _)| *t);
            let Some((_, pool)) = victim else { break };
            if let Some(bytes) = pool.evict_oldest() {
                freed += bytes;
            }
        }
        freed
    }
}
