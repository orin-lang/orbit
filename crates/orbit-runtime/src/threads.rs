//! Guest threads: `orbit:thread/v1` (ADR-0012, `docs/host-abi.md` section 4f).
//!
//! A threaded module imports its linear memory as a shared `env.memory`. Orbit creates one
//! [`SharedMemory`] per isolate, and every guest thread is a **separate instance of the same
//! module on that memory**, in its own `Store` but with the isolate's grants and capability
//! state, running on **its own OS thread** (a thread may block in `memory.atomic.wait32`, which
//! blocks the OS thread, so a thread never occupies a Tokio worker).
//!
//! # A run
//!
//! One *run* is one `Isolate::init` / `invoke` call of a threaded isolate (the *main thread*).
//! The main thread executes on a dedicated OS thread too; the calling task only awaits its
//! result. When the main thread returns, the run ends: every thread that is still alive is
//! terminated, and the call returns after all of them are gone.
//!
//! # Termination
//!
//! Any thread that traps, exits, runs out of fuel or hits the deadline ends all of them. The
//! first failure is recorded and the isolate-wide phase goes from `RUNNING` to `TERMINATING`.
//! Every thread is then stopped by whichever of three mechanisms applies to where it is:
//!
//! 1. *running wasm*: the epoch callback (every store checks it at the next epoch tick, at
//!    most one tick later; `Engine::increment_epoch` is called to make that immediate);
//! 2. *suspended in an async host call* (a socket wait): the thread's future is raced against
//!    the termination signal and dropped;
//! 3. *blocked in `memory.atomic.wait32/64`*: nothing in Wasmtime can interrupt a wait except
//!    a notify on the exact address, and the host does not know the address. The supervisor
//!    therefore **notifies every 4-byte-aligned address of the memory**, repeatedly, until no
//!    thread is left. Cost: O(memory size) per pass (a few tens of milliseconds for 16 MiB).
//!    A woken thread continues, reaches its next loop header or call and is stopped by (1).
//!
//! The supervisor also enforces the wall-clock deadline for threads blocked in a wait (the
//! epoch callback cannot run in them): at the deadline it terminates the isolate with
//! [`ExecError::DeadlineExceeded`].

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use orbit_abi::{THREAD_START_EXPORT, status};
use tokio::runtime::Handle;
use tokio::sync::Notify;
use wasmtime::{
    Func, InstancePre, SharedMemory, Store, StoreContextMut, Trap, UpdateDeadline, Val,
};

use crate::capability::{Capability, Grants, InstanceInfo};
use crate::error::ExecError;
use crate::host::{HostMetrics, HostState};
use crate::isolate::classify_error;
use crate::limits::Limits;

/// No run in progress and none was cancelled.
const IDLE: u8 = 0;
/// A run is in progress.
const RUNNING: u8 = 1;
/// The run is ending: every thread must stop.
const TERMINATING: u8 = 2;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The epoch callback of every store of a threaded or plain isolate.
///
/// Runs on every epoch tick: ends the store when the isolate is terminating or the absolute
/// deadline passed, otherwise yields to the executor so a busy guest cannot monopolise it.
pub(crate) fn epoch_callback(
    ctx: StoreContextMut<'_, HostState>,
) -> wasmtime::Result<UpdateDeadline> {
    let host = ctx.data();
    if host.observe_termination && host.threads.as_ref().is_some_and(|t| t.is_terminating()) {
        return Ok(UpdateDeadline::Interrupt);
    }
    match host.deadline {
        Some(d) if Instant::now() >= d => Ok(UpdateDeadline::Interrupt),
        _ => Ok(UpdateDeadline::Yield(1)),
    }
}

struct Sync {
    /// Guest threads alive besides the main thread.
    live: usize,
    /// The main thread's call is still executing (it may be blocked in a wait too).
    main_running: bool,
    /// The main thread has finished the run; the supervisor must exit.
    done: bool,
}

struct Session {
    handle: Handle,
    deadline: Option<Instant>,
}

/// Everything `Isolate::create` hands over.
pub(crate) struct ThreadParams {
    pub(crate) memory: SharedMemory,
    pub(crate) engine: wasmtime::Engine,
    pub(crate) pre: InstancePre<HostState>,
    pub(crate) caps: Vec<Arc<dyn Capability>>,
    pub(crate) info: InstanceInfo,
    pub(crate) limits: Limits,
    pub(crate) metered: bool,
    pub(crate) max_threads: usize,
    /// How long the supervisor lets the epoch mechanism work before it sweeps the memory.
    pub(crate) grace: Duration,
}

/// The thread state of one isolate, shared by all of its threads (and its supervisor).
pub(crate) struct ThreadShared {
    pub(crate) memory: SharedMemory,
    engine: wasmtime::Engine,
    pre: InstancePre<HostState>,
    caps: Vec<Arc<dyn Capability>>,
    info: InstanceInfo,
    limits: Limits,
    metered: bool,
    max_threads: usize,
    grace: Duration,

    phase: AtomicU8,
    /// Wakes `terminated()` futures.
    wake: Notify,
    sync: Mutex<Sync>,
    cv: Condvar,
    session: Mutex<Option<Session>>,
    /// The first failure of the run, if it was not the main thread's own error.
    failure: Mutex<Option<ExecError>>,
    next_tid: AtomicU64,
    spawned: AtomicU64,
    fuel_used: AtomicU64,
    metrics: Mutex<HostMetrics>,
    handles: Mutex<Vec<JoinHandle<()>>>,
}

/// Decrements the live-thread count when a guest thread ends, however it ends.
struct LiveGuard(Arc<ThreadShared>);

impl Drop for LiveGuard {
    fn drop(&mut self) {
        let mut s = lock(&self.0.sync);
        s.live = s.live.saturating_sub(1);
        drop(s);
        self.0.cv.notify_all();
    }
}

/// Terminates the run when the future that awaits it is dropped.
struct CancelGuard(Option<Arc<ThreadShared>>);

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if let Some(t) = self.0.take() {
            t.terminate(None);
        }
    }
}

impl ThreadShared {
    pub(crate) fn new(p: ThreadParams) -> Arc<Self> {
        Arc::new(Self {
            memory: p.memory,
            engine: p.engine,
            pre: p.pre,
            caps: p.caps,
            info: p.info,
            limits: p.limits,
            metered: p.metered,
            max_threads: p.max_threads,
            grace: p.grace,
            phase: AtomicU8::new(IDLE),
            wake: Notify::new(),
            sync: Mutex::new(Sync {
                live: 0,
                main_running: false,
                done: false,
            }),
            cv: Condvar::new(),
            session: Mutex::new(None),
            failure: Mutex::new(None),
            next_tid: AtomicU64::new(1),
            spawned: AtomicU64::new(0),
            fuel_used: AtomicU64::new(0),
            metrics: Mutex::new(HostMetrics::default()),
            handles: Mutex::new(Vec::new()),
        })
    }

    fn phase(&self) -> u8 {
        self.phase.load(Ordering::SeqCst)
    }

    pub(crate) fn is_terminating(&self) -> bool {
        self.phase() == TERMINATING
    }

    /// Threads spawned over the isolate's life (exact).
    pub(crate) fn spawned(&self) -> u64 {
        self.spawned.load(Ordering::Relaxed)
    }

    /// Fuel consumed by the spawned threads since the last call (exact; 0 when unmetered).
    pub(crate) fn take_fuel(&self) -> u64 {
        self.fuel_used.swap(0, Ordering::Relaxed)
    }

    /// Host-call counters of the finished spawned threads.
    pub(crate) fn thread_metrics(&self) -> HostMetrics {
        lock(&self.metrics).clone()
    }

    /// Ends the current run: the first caller records `reason` (when there is one) and wakes
    /// every mechanism that stops threads. Later calls, and calls outside a run, do nothing.
    pub(crate) fn terminate(&self, reason: Option<ExecError>) -> bool {
        if self
            .phase
            .compare_exchange(RUNNING, TERMINATING, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return false;
        }
        if let Some(r) = reason {
            *lock(&self.failure) = Some(r);
        }
        self.wake.notify_waiters();
        // Every store's epoch deadline is one tick ahead: advancing the epoch now makes
        // running threads call the epoch callback immediately instead of at the next tick.
        self.engine.increment_epoch();
        let _s = lock(&self.sync);
        self.cv.notify_all();
        true
    }

    /// Resolves once the run is terminating.
    async fn terminated(&self) {
        loop {
            let n = self.wake.notified();
            if self.is_terminating() {
                return;
            }
            n.await;
        }
    }

    fn begin_session(&self, handle: Handle, deadline: Option<Instant>) {
        *lock(&self.failure) = None;
        self.fuel_used.store(0, Ordering::Relaxed);
        {
            let mut s = lock(&self.sync);
            s.done = false;
            s.main_running = true;
        }
        *lock(&self.session) = Some(Session { handle, deadline });
        self.phase.store(RUNNING, Ordering::SeqCst);
    }

    /// Implements the guest's `spawn(arg)`; called from the `orbit:thread/v1` host function with
    /// the caller's host state.
    pub(crate) fn spawn(self: &Arc<Self>, parent: &mut HostState, arg: i64) -> i64 {
        if self.max_threads == 0 {
            return i64::from(status::ERR_DENIED);
        }
        let (handle, deadline) = match lock(&self.session).as_ref() {
            Some(s) => (s.handle.clone(), s.deadline),
            None => return i64::from(status::ERR_CANCELLED),
        };
        {
            let mut s = lock(&self.sync);
            if self.phase() != RUNNING {
                return i64::from(status::ERR_CANCELLED);
            }
            if s.live >= self.max_threads {
                return i64::from(status::ERR_QUOTA);
            }
            s.live += 1;
        }
        let live = LiveGuard(Arc::clone(self));
        let tid = self.next_tid.fetch_add(1, Ordering::Relaxed);

        // The new thread's capability state is derived from the spawning thread's, so shared
        // resources (sockets, files, budgets) stay shared and per-thread ones start fresh.
        let mut host = HostState::new(self.info.clone(), &self.limits);
        let null = serde_json::Value::Null;
        for cap in &self.caps {
            let id = cap.id();
            let config = self.info.grants.config(&id).unwrap_or(&null);
            let state = match parent.cap_state_dyn(&id) {
                Some(p) => cap.thread_state(p, &self.info, config),
                None => cap.instance_state(&self.info, config),
            };
            host.insert_cap_state(id, state);
        }
        host.threads = Some(Arc::clone(self));
        host.shared_memory = Some(self.memory.clone());
        host.deadline = deadline;
        host.observe_termination = true;

        let me = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name(format!("orbit-guest-{tid}"))
            .spawn(move || {
                let _live = live;
                let r = catch_unwind(AssertUnwindSafe(|| {
                    handle.block_on(me.run_thread(tid, arg, host));
                }));
                if r.is_err() {
                    me.terminate(Some(ExecError::Trap(format!(
                        "thread {tid}: internal error (host panic)"
                    ))));
                }
            });
        match spawned {
            Ok(j) => {
                let mut hs = lock(&self.handles);
                hs.retain(|h| !h.is_finished());
                hs.push(j);
                self.spawned.fetch_add(1, Ordering::Relaxed);
                tid as i64
            }
            // `live` was dropped with the closure: the slot is free again.
            Err(_) => i64::from(status::ERR_QUOTA),
        }
    }

    /// The body of one spawned guest thread (on its own OS thread, inside `block_on`).
    async fn run_thread(&self, tid: u64, arg: i64, host: HostState) {
        let mut store = Store::new(&self.engine, host);
        store.limiter(|h| &mut h.limiter);
        store.epoch_deadline_callback(epoch_callback);
        store.set_epoch_deadline(1);
        if self.metered {
            let _ = store.set_fuel(self.limits.fuel.unwrap_or(u64::MAX));
        }
        let outcome = {
            let run = async {
                let instance = self.pre.instantiate_async(&mut store).await?;
                let start =
                    instance.get_typed_func::<(i64, i64), ()>(&mut store, THREAD_START_EXPORT)?;
                start.call_async(&mut store, (tid as i64, arg)).await
            };
            tokio::select! {
                biased;
                () = self.terminated() => None,
                r = run => Some(r),
            }
        };
        if self.metered {
            let left = store.get_fuel().unwrap_or(0);
            let start = self.limits.fuel.unwrap_or(u64::MAX);
            self.fuel_used
                .fetch_add(start.saturating_sub(left), Ordering::Relaxed);
        }
        let host = store.data_mut();
        for cap in &self.caps {
            if let Some(s) = host.cap_state_dyn(&cap.id()) {
                cap.end_invocation(s);
            }
        }
        lock(&self.metrics).merge(host.metrics());
        if let Some(Err(e)) = outcome {
            let mut exec = classify_error(&e, 0);
            if let ExecError::Trap(m) = &exec {
                exec = ExecError::Trap(format!("thread {tid}: {m}"));
            }
            self.terminate(Some(exec));
        }
    }

    /// Runs the main thread of one invocation on a dedicated OS thread and returns when it and
    /// every thread it spawned are gone. The store goes to the thread and comes back; it is
    /// `None` only if the thread could not be started or panicked.
    pub(crate) async fn run_main(
        self: &Arc<Self>,
        mut store: Store<HostState>,
        func: Func,
        params: Vec<Val>,
    ) -> (Option<Store<HostState>>, wasmtime::Result<Vec<Val>>) {
        let handle = Handle::current();
        self.begin_session(handle.clone(), store.data().deadline);
        store.data_mut().observe_termination = true;
        let nresults = func.ty(&store).results().len();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut cancel = CancelGuard(Some(Arc::clone(self)));
        let me = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("orbit-guest-main".into())
            .spawn(move || {
                let out = me.main_thread(&handle, store, func, &params, nresults);
                let _ = tx.send(out);
            });
        if let Err(e) = spawned {
            return (
                None,
                Err(wasmtime::Error::msg(format!(
                    "cannot start the guest main thread: {e}"
                ))),
            );
        }
        let out = rx.await;
        cancel.0 = None;
        match out {
            Ok((s, r)) => (Some(s), r),
            Err(_) => (
                None,
                Err(wasmtime::Error::msg("the guest main thread panicked")),
            ),
        }
    }

    fn main_thread(
        self: &Arc<Self>,
        handle: &Handle,
        mut store: Store<HostState>,
        func: Func,
        params: &[Val],
        nresults: usize,
    ) -> (Store<HostState>, wasmtime::Result<Vec<Val>>) {
        let deadline = store.data().deadline;
        let supervisor = {
            let me = Arc::clone(self);
            std::thread::Builder::new()
                .name("orbit-supervisor".into())
                .spawn(move || me.supervise(deadline))
                .ok()
        };
        let result = catch_unwind(AssertUnwindSafe(|| {
            handle.block_on(async {
                let mut results = vec![Val::I32(0); nresults];
                let r = tokio::select! {
                    biased;
                    () = self.terminated() => Err(wasmtime::Error::new(Trap::Interrupt)),
                    r = func.call_async(&mut store, params, &mut results) => r,
                };
                r.map(|()| results)
            })
        }))
        .unwrap_or_else(|_| Err(wasmtime::Error::msg("the guest main thread panicked")));

        // The run is over: whatever is still alive is terminated. If the main thread failed
        // itself, its own error is returned (the first failure wins; this call is then a
        // no-op when another thread was first).
        lock(&self.sync).main_running = false;
        self.cv.notify_all();
        self.terminate(None);
        {
            let mut s = lock(&self.sync);
            while s.live > 0 {
                s = self
                    .cv
                    .wait(s)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            s.done = true;
        }
        self.cv.notify_all();
        if let Some(h) = supervisor {
            let _ = h.join();
        }
        let threads: Vec<_> = std::mem::take(&mut *lock(&self.handles));
        for h in threads {
            let _ = h.join();
        }
        store.data_mut().observe_termination = false;
        let result = match lock(&self.failure).take() {
            Some(f) => Err(wasmtime::Error::new(f)),
            None => result,
        };
        (store, result)
    }

    /// The watchdog of a run: enforces the deadline for threads that cannot check it (blocked
    /// in a wait) and, once the run is terminating, wakes the threads blocked in a wait.
    fn supervise(self: &Arc<Self>, deadline: Option<Instant>) {
        let mut s = lock(&self.sync);
        loop {
            if s.done {
                return;
            }
            if self.phase() == RUNNING {
                match deadline {
                    Some(d) if Instant::now() >= d => {
                        drop(s);
                        self.terminate(Some(ExecError::DeadlineExceeded));
                        s = lock(&self.sync);
                    }
                    Some(d) => {
                        s = self
                            .cv
                            .wait_timeout(s, d.saturating_duration_since(Instant::now()))
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .0;
                    }
                    None => {
                        s = self
                            .cv
                            .wait(s)
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                    }
                }
            } else if s.live == 0 && !s.main_running {
                // Nothing to wake; wait for the main thread to finish the run.
                s = self
                    .cv
                    .wait(s)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            } else {
                // Terminating with threads alive: give the epoch mechanism a moment, then
                // wake every possible waiter, again and again until the threads are gone.
                let (g, _) = self
                    .cv
                    .wait_timeout_while(s, self.grace, |s| {
                        (s.live > 0 || s.main_running) && !s.done
                    })
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                s = g;
                if s.done || (s.live == 0 && !s.main_running) {
                    continue;
                }
                drop(s);
                self.sweep();
                s = lock(&self.sync);
            }
        }
    }

    /// Notifies every 4-byte-aligned address of the memory, waking every thread blocked in
    /// `memory.atomic.wait32` or `wait64` (a 64-bit waiter's address is 8-byte aligned, so it is
    /// a subset). Stops early when the last thread is gone.
    fn sweep(&self) {
        let size = self.memory.data_size() as u64;
        let mut addr = 0u64;
        while addr + 4 <= size {
            let _ = self.memory.atomic_notify(addr, u32::MAX);
            addr += 4;
            if addr & ((1 << 20) - 1) == 0 && {
                let s = lock(&self.sync);
                s.live == 0 && !s.main_running
            } {
                return;
            }
        }
    }
}

/// Grants of an isolate whose `thread/v1` configuration is `{"max_threads": N}`.
pub(crate) fn max_threads_of(grants: &Grants) -> usize {
    use orbit_abi::{CapabilityId, limits as lim};
    grants
        .config(&CapabilityId::thread_v1())
        .and_then(|c| c.get("max_threads"))
        .and_then(serde_json::Value::as_u64)
        .map_or(lim::THREAD_DEFAULT_MAX_THREADS, |n| {
            usize::try_from(n)
                .unwrap_or(usize::MAX)
                .min(lim::THREAD_MAX_THREADS_CEILING)
        })
}
