//! A single isolate: one instance of an artifact with its own store, memory and state.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use orbit_abi::{CapabilityId, INIT_EXPORT, INITIALIZE_EXPORT, MEMORY_EXPORT};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use wasmtime::{Func, Instance, Memory, Store, Trap, UpdateDeadline, Val, ValType};

use crate::IsolateState;
use crate::artifact::Artifact;
use crate::capability::{Capability, Grants, InstanceId, InstanceInfo, TenantId};
use crate::error::{ExecError, GuestExit, InstantiateError};
use crate::host::HostState;
use crate::lifecycle::Lifecycle;
use crate::limits::Limits;
use crate::runtime::MemoryReservation;

/// Measurements of one finished invocation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InvocationMetrics {
    /// Fuel consumed (measured: difference of `Store::get_fuel` before and after), or `None`
    /// when the engine has no fuel metering (`EngineConfig::fuel_metering == false`): fuel is
    /// then *not measured*, which is different from "measured as 0".
    pub fuel_consumed: Option<u64>,
    /// Wall-clock time from acquiring the run slot to the guest returning or trapping
    /// (measured with `Instant`; includes time spent yielded to the executor).
    pub wall_time: Duration,
}

/// Snapshot of an isolate's metrics.
///
/// Measured values come straight from Wasmtime or the host; there are no estimates in this
/// struct. `fuel_consumed` is Wasmtime's own instruction-based fuel, not CPU time, and is only
/// available when the engine was built with `EngineConfig::fuel_metering`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IsolateMetrics {
    /// Lifecycle state.
    pub state: IsolateState,
    /// Total fuel consumed over the isolate's life (measured), or `None` when fuel metering
    /// is disabled in the engine (not measured; never reported as 0).
    pub fuel_consumed: Option<u64>,
    /// Metrics of the most recent finished invocation or initialisation (measured).
    pub last_invocation: Option<InvocationMetrics>,
    /// Number of finished invocations, not counting initialisation (exact).
    pub invocations: u64,
    /// Time to build the store and instantiate, including admission checks (measured).
    pub instantiation_latency: Duration,
    /// Current linear memory size in bytes (measured from the live memory when the module
    /// exports `memory`, otherwise as reported to the limiter).
    pub memory_bytes: u64,
    /// Peak linear memory size in bytes as seen by the limiter (measured; sampled at growth
    /// requests, which is exactly when the size changes).
    pub peak_memory_bytes: u64,
    /// `memory.grow` requests refused by `Limits::max_memory_bytes` (exact).
    pub memory_grow_denied: u64,
    /// `table.grow` requests refused by `Limits::max_table_elements` (exact).
    pub table_grow_denied: u64,
    /// Host function calls per capability (exact).
    pub host_calls: HashMap<CapabilityId, u64>,
    /// Bytes accepted by the log capability (exact).
    pub bytes_logged: u64,
}

/// Everything `Runtime::spawn_isolate` decides before the isolate is built.
pub(crate) struct SpawnParams {
    pub(crate) id: InstanceId,
    pub(crate) tenant: TenantId,
    pub(crate) limits: Limits,
    pub(crate) semaphore: Arc<Semaphore>,
    pub(crate) reservation: MemoryReservation,
}

const UNMETERED: u64 = u64::MAX;
const POST_MORTEM_FUEL: u64 = 100_000;
const POST_MORTEM_TIMEOUT: Duration = Duration::from_millis(250);

/// One running instance of an [`Artifact`].
///
/// An `Isolate` is `Send`; drive it from any task. Methods take `&mut self`, so an isolate
/// runs at most one thing at a time.
pub struct Isolate {
    id: InstanceId,
    tenant: TenantId,
    state: Lifecycle,
    store: Option<Store<HostState>>,
    instance: Option<Instance>,
    memory: Option<Memory>,
    caps: Vec<Arc<dyn Capability>>,
    limits: Limits,
    metered: bool,
    semaphore: Arc<Semaphore>,
    reservation: Option<MemoryReservation>,
    failure: Option<ExecError>,
    exit_code: Option<i32>,
    fuel_total: u64,
    invocations: u64,
    last: Option<InvocationMetrics>,
    instantiation_latency: Duration,
    // Final readings kept after the store is dropped by `terminate`.
    final_metrics: Option<IsolateMetrics>,
}

impl Isolate {
    pub(crate) async fn create(
        artifact: &Artifact,
        grants: &Grants,
        engine: &crate::OrbitEngine,
        params: SpawnParams,
    ) -> Result<Self, InstantiateError> {
        let SpawnParams {
            id,
            tenant,
            limits,
            semaphore,
            reservation,
        } = params;
        let metered = engine.fuel_metering();
        if limits.fuel.is_some() && !metered {
            return Err(InstantiateError::FuelMeteringDisabled);
        }
        let started = Instant::now();
        let info = InstanceInfo {
            instance_id: id,
            tenant: tenant.clone(),
            max_log_bytes: limits.max_log_bytes,
            grants: grants.clone(),
        };
        let mut host = HostState::new(info.clone(), &limits);
        for cap in artifact.linked() {
            let cid = cap.id();
            let null = serde_json::Value::Null;
            let state = cap.instance_state(&info, grants.config(&cid).unwrap_or(&null));
            host.insert_cap_state(cid, state);
        }

        let mut store = Store::new(engine.wasmtime(), host);
        store.limiter(|h| &mut h.limiter);
        store.epoch_deadline_callback(|ctx| {
            // Runs on every epoch tick: enforce the absolute deadline, otherwise yield to
            // the executor so one isolate cannot monopolise a worker thread.
            match ctx.data().deadline {
                Some(d) if Instant::now() >= d => Ok(UpdateDeadline::Interrupt),
                _ => Ok(UpdateDeadline::Yield(1)),
            }
        });
        arm(&mut store, &limits, metered)
            .map_err(|e| InstantiateError::Instantiate(format!("{e:#}")))?;

        let instantiated = artifact.pre().instantiate_async(&mut store).await;
        let fuel_used = if metered {
            limits
                .fuel
                .unwrap_or(UNMETERED)
                .saturating_sub(store.get_fuel().unwrap_or(0))
        } else {
            0
        };
        store.data_mut().deadline = None;
        let instance = match instantiated {
            Ok(i) => i,
            Err(e) => {
                let e = if metered {
                    e.context(format!(
                        "instantiation used {fuel_used} fuel of {}",
                        limits
                            .fuel
                            .map_or_else(|| "unlimited".to_owned(), |f| f.to_string())
                    ))
                } else {
                    e
                };
                let lim = &store.data().limiter;
                return Err(if lim.memory_grow_denied + lim.table_grow_denied > 0 {
                    InstantiateError::ResourceLimit(format!("{e:#}"))
                } else {
                    InstantiateError::Instantiate(format!("{e:#}"))
                });
            }
        };

        let memory = instance.get_memory(&mut store, MEMORY_EXPORT);
        Ok(Self {
            id,
            tenant,
            state: Lifecycle::new(),
            store: Some(store),
            memory,
            instance: Some(instance),
            caps: artifact.linked().to_vec(),
            limits,
            metered,
            semaphore,
            reservation: Some(reservation),
            failure: None,
            exit_code: None,
            fuel_total: fuel_used,
            invocations: 0,
            last: None,
            instantiation_latency: started.elapsed(),
            final_metrics: None,
        })
    }

    /// The isolate's id.
    #[must_use]
    pub fn id(&self) -> InstanceId {
        self.id
    }

    /// The owning tenant.
    #[must_use]
    pub fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    /// Current lifecycle state.
    #[must_use]
    pub fn state(&self) -> IsolateState {
        self.state.get()
    }

    /// The error that put the isolate into [`IsolateState::Failed`], if any.
    #[must_use]
    pub fn failure(&self) -> Option<&ExecError> {
        self.failure.as_ref()
    }

    /// The code of the guest's `proc_exit` call, if it made one. `proc_exit(0)` leaves the
    /// isolate [`Terminated`](IsolateState::Terminated) and the call that made it returns
    /// `Ok`; a non-zero code leaves it [`Failed`](IsolateState::Failed) with
    /// [`ExecError::Exit`].
    #[must_use]
    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    /// The limits this isolate was created with.
    #[must_use]
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Runs `_initialize` and then `orbit_init`, when exported.
    ///
    /// A trap, a limit violation or a non-zero result from `orbit_init` moves the isolate to
    /// [`IsolateState::Failed`] and is returned.
    ///
    /// # Errors
    /// [`ExecError`]; [`ExecError::InvalidState`] unless the isolate is `Created`.
    pub async fn init(&mut self) -> Result<(), ExecError> {
        if self.state() != IsolateState::Created {
            return Err(ExecError::InvalidState(self.state()));
        }
        let (init, orbit_init) = {
            let store = self.store.as_mut().expect("live isolate has a store");
            let instance = self.instance.expect("live isolate has an instance");
            (
                instance.get_func(&mut *store, INITIALIZE_EXPORT),
                instance.get_func(&mut *store, INIT_EXPORT),
            )
        };
        let plan: Vec<(Func, bool)> = [(init, false), (orbit_init, true)]
            .into_iter()
            .filter_map(|(f, o)| Some((f?, o)))
            .collect();
        // Validate shapes before running anything.
        let store = self.store.as_ref().expect("live isolate has a store");
        for (f, _) in &plan {
            let ty = f.ty(store);
            if ty.params().len() != 0 || ty.results().len() > 1 {
                let e = ExecError::BadArguments(
                    "`_initialize`/`orbit_init` must take no parameters and return nothing or an i32"
                        .into(),
                );
                self.fail(e.clone());
                return Err(e);
            }
        }
        let permit = self.begin().await?;
        let start = Instant::now();
        let fuel_before = self.fuel_now();
        let denied_before = self.denied_total();
        let mut outcome: Result<(), ExecError> = Ok(());
        for (f, is_orbit_init) in plan {
            match self.call(f, &[]).await {
                Ok(results) => {
                    if let (true, Some(Val::I32(code))) = (is_orbit_init, results.first())
                        && *code != 0
                    {
                        outcome = Err(ExecError::InitFailed(*code));
                        break;
                    }
                }
                Err(e) => {
                    outcome = Err(self.classify(&e, denied_before));
                    break;
                }
            }
        }
        drop(permit);
        self.finish(
            start,
            fuel_before,
            outcome,
            IsolateState::Initialized,
            false,
        )
    }

    /// Calls the exported function `name` with integer arguments.
    ///
    /// `i32` parameters accept any value that fits in `i32` or `u32`; results are
    /// sign-extended to `i64`. Only integer parameters and results are supported.
    /// A trap, a fuel or deadline exhaustion moves the isolate to [`IsolateState::Failed`].
    ///
    /// # Errors
    /// [`ExecError::InvalidState`] unless the isolate is `Initialized` or `Idle`;
    /// [`ExecError::NoSuchExport`] / [`ExecError::BadArguments`] (the isolate stays usable);
    /// otherwise the failure of the guest.
    pub async fn invoke(&mut self, name: &str, args: &[i64]) -> Result<Vec<i64>, ExecError> {
        if !self.state().accepts_invocation() {
            return Err(ExecError::InvalidState(self.state()));
        }
        let (func, params, nresults) = {
            let store = self.store.as_mut().expect("live isolate has a store");
            let instance = self.instance.expect("live isolate has an instance");
            let func = instance
                .get_func(&mut *store, name)
                .ok_or_else(|| ExecError::NoSuchExport(name.to_owned()))?;
            let ty = func.ty(&*store);
            if ty.params().len() != args.len() {
                return Err(ExecError::BadArguments(format!(
                    "`{name}` takes {} argument(s), got {}",
                    ty.params().len(),
                    args.len()
                )));
            }
            let mut params = Vec::with_capacity(args.len());
            for (i, (p, a)) in ty.params().zip(args).enumerate() {
                params.push(match p {
                    ValType::I32 => Val::I32(
                        i32::try_from(*a)
                            .or_else(|_| u32::try_from(*a).map(|u| u as i32))
                            .map_err(|_| {
                                ExecError::BadArguments(format!("argument {i} does not fit i32"))
                            })?,
                    ),
                    ValType::I64 => Val::I64(*a),
                    other => {
                        return Err(ExecError::BadArguments(format!(
                            "parameter {i} has unsupported type {other}"
                        )));
                    }
                });
            }
            if let Some(bad) = ty
                .results()
                .find(|r| !matches!(r, ValType::I32 | ValType::I64))
            {
                return Err(ExecError::BadArguments(format!(
                    "`{name}` returns unsupported type {bad}"
                )));
            }
            (func, params, ty.results().len())
        };

        let permit = self.begin().await?;
        let start = Instant::now();
        let fuel_before = self.fuel_now();
        let denied_before = self.denied_total();
        let result = self.call(func, &params).await;
        drop(permit);
        let outcome = result.map_err(|e| self.classify(&e, denied_before));
        let values = match &outcome {
            Ok(vals) => {
                let out: Vec<i64> = vals
                    .iter()
                    .map(|v| match v {
                        Val::I32(i) => i64::from(*i),
                        Val::I64(i) => *i,
                        _ => 0,
                    })
                    .collect();
                debug_assert_eq!(out.len(), nresults);
                out
            }
            Err(_) => Vec::new(),
        };
        self.finish(
            start,
            fuel_before,
            outcome.map(|_| ()),
            IsolateState::Idle,
            true,
        )?;
        Ok(values)
    }

    /// The per-isolate state of capability `id`, if it has state of type `T`. A host uses it
    /// to reach a capability's control surface from outside the guest (for example to signal a
    /// `orbit:net/v1` wake source while the isolate is busy in a wait). `None` after
    /// [`terminate`](Self::terminate).
    pub fn cap_state_mut<T: std::any::Any>(&mut self, id: &CapabilityId) -> Option<&mut T> {
        self.store.as_mut()?.data_mut().cap_state_mut::<T>(id)
    }

    /// Reads the program's exit status from a guest export after a trap.
    ///
    /// Some guests (for example Orin programs, see `docs/host-abi.md` section 5) record the
    /// exit status the program chose before it trapped, and expose it through an export
    /// `() -> i32`. This is the only way to run guest code in [`IsolateState::Failed`], and it
    /// is deliberately narrow:
    ///
    /// * allowed only when the isolate is `Failed` because of [`ExecError::Trap`]. A fuel,
    ///   deadline, memory-limit, `proc_exit` or `orbit_init` failure returns `None`: the
    ///   guest state is not trustworthy or the limits were already hit;
    /// * the export must have exactly the type `() -> i32`; otherwise `None`;
    /// * it runs under its own small budget (100 000 fuel on a metered engine, and a deadline
    ///   of the invocation timeout capped at 250 ms), holding a run slot like any invocation;
    /// * it does not reset per-invocation capability budgets (the log budget stays spent), does
    ///   not count as an invocation and **never changes the state**: the isolate stays
    ///   `Failed` and [`failure`](Self::failure) keeps the original trap, also when this call
    ///   itself traps (that returns `None`).
    ///
    /// Returns `Some(status)` on success.
    pub async fn post_mortem_i32(&mut self, export: &str) -> Option<i32> {
        if self.state() != IsolateState::Failed || !matches!(self.failure, Some(ExecError::Trap(_)))
        {
            return None;
        }
        let func = {
            let store = self.store.as_mut()?;
            let func = self.instance?.get_func(&mut *store, export)?;
            let ty = func.ty(&*store);
            if ty.params().len() != 0
                || ty.results().len() != 1
                || !ty.results().all(|r| matches!(r, ValType::I32))
            {
                return None;
            }
            func
        };
        let permit = Arc::clone(&self.semaphore).acquire_owned().await.ok()?;
        let timeout = self
            .limits
            .timeout
            .map_or(POST_MORTEM_TIMEOUT, |t| t.min(POST_MORTEM_TIMEOUT));
        let armed = arm_with(
            self.store.as_mut()?,
            Some(POST_MORTEM_FUEL),
            Some(timeout),
            self.metered,
        );
        let result = if armed.is_ok() {
            self.call(func, &[]).await.ok()
        } else {
            None
        };
        drop(permit);
        let fuel_left = self.fuel_now();
        if let Some(store) = self.store.as_mut() {
            store.data_mut().deadline = None;
            for cap in &self.caps {
                if let Some(s) = store.data_mut().cap_state_dyn(&cap.id()) {
                    cap.end_invocation(s);
                }
            }
        }
        if self.metered {
            self.fuel_total = self
                .fuel_total
                .saturating_add(POST_MORTEM_FUEL.saturating_sub(fuel_left));
        }
        match result?.first() {
            Some(Val::I32(v)) => Some(*v),
            _ => None,
        }
    }

    /// Stops the isolate and frees everything it owns: the store (so its linear memory,
    /// tables and handles), the per-capability state and the memory admission reservation.
    /// Idempotent.
    pub fn terminate(&mut self) {
        if self.state() == IsolateState::Terminated {
            return;
        }
        // Live states can always move to Stopping and then Terminated.
        let _ = self.state.transition(IsolateState::Stopping);
        self.final_metrics = Some(self.metrics());
        self.instance = None;
        self.memory = None;
        self.store = None;
        self.caps.clear();
        self.reservation = None;
        let _ = self.state.transition(IsolateState::Terminated);
        if let Some(m) = &mut self.final_metrics {
            m.state = IsolateState::Terminated;
        }
    }

    /// A snapshot of the isolate's metrics. After [`terminate`](Self::terminate) it returns the
    /// last readings taken before teardown.
    #[must_use]
    pub fn metrics(&self) -> IsolateMetrics {
        if let Some(m) = &self.final_metrics {
            return m.clone();
        }
        let store = self.store.as_ref().expect("live isolate has a store");
        let host = store.data();
        let mem_export = self.memory.map(|m| m.data_size(store) as u64);
        IsolateMetrics {
            state: self.state(),
            fuel_consumed: self.metered.then_some(self.fuel_total),
            last_invocation: self.last,
            invocations: self.invocations,
            instantiation_latency: self.instantiation_latency,
            memory_bytes: mem_export.unwrap_or(host.limiter.memory_bytes),
            peak_memory_bytes: host.limiter.peak_memory_bytes.max(mem_export.unwrap_or(0)),
            memory_grow_denied: host.limiter.memory_grow_denied,
            table_grow_denied: host.limiter.table_grow_denied,
            host_calls: host.metrics().calls.clone(),
            bytes_logged: host.metrics().bytes_logged,
        }
    }

    // --- internals -------------------------------------------------------------------

    fn fail(&mut self, e: ExecError) {
        let _ = self.state.transition(IsolateState::Failed);
        self.failure = Some(e);
    }

    fn fuel_now(&self) -> u64 {
        if !self.metered {
            return 0;
        }
        self.store
            .as_ref()
            .and_then(|s| s.get_fuel().ok())
            .unwrap_or(0)
    }

    fn denied_total(&self) -> u64 {
        self.store.as_ref().map_or(0, |s| {
            let l = &s.data().limiter;
            l.memory_grow_denied + l.table_grow_denied
        })
    }

    /// Acquires a run slot, moves to `Running`, resets per-invocation budgets and arms the
    /// fuel and deadline limits.
    async fn begin(&mut self) -> Result<OwnedSemaphorePermit, ExecError> {
        let permit = Arc::clone(&self.semaphore)
            .acquire_owned()
            .await
            .map_err(|_| ExecError::Closed)?;
        self.state
            .transition(IsolateState::Running)
            .map_err(|_| ExecError::InvalidState(self.state()))?;
        let store = self.store.as_mut().expect("live isolate has a store");
        for cap in &self.caps {
            if let Some(s) = store.data_mut().cap_state_dyn(&cap.id()) {
                cap.begin_invocation(s);
            }
        }
        if let Err(e) = arm(store, &self.limits, self.metered) {
            let e = ExecError::Trap(format!("{e:#}"));
            self.fail(e.clone());
            return Err(e);
        }
        Ok(permit)
    }

    async fn call(&mut self, func: Func, params: &[Val]) -> wasmtime::Result<Vec<Val>> {
        let store = self.store.as_mut().expect("live isolate has a store");
        let mut results = vec![Val::I32(0); func.ty(&*store).results().len()];
        func.call_async(&mut *store, params, &mut results).await?;
        Ok(results)
    }

    fn classify(&self, e: &wasmtime::Error, denied_before: u64) -> ExecError {
        if let Some(GuestExit(code)) = e.downcast_ref::<GuestExit>() {
            return ExecError::Exit(*code);
        }
        match e.downcast_ref::<Trap>() {
            Some(Trap::OutOfFuel) => ExecError::OutOfFuel,
            Some(Trap::Interrupt) => ExecError::DeadlineExceeded,
            _ => {
                let denied = self.denied_total().saturating_sub(denied_before);
                if denied > 0 {
                    ExecError::MemoryLimit {
                        denied,
                        trap: format!("{e:#}"),
                    }
                } else {
                    ExecError::Trap(format!("{e:#}"))
                }
            }
        }
    }

    /// Records measurements and performs the post-run state transition.
    fn finish(
        &mut self,
        start: Instant,
        fuel_before: u64,
        outcome: Result<(), ExecError>,
        ok_state: IsolateState,
        count: bool,
    ) -> Result<(), ExecError> {
        let consumed = fuel_before.saturating_sub(self.fuel_now());
        self.fuel_total = self.fuel_total.saturating_add(consumed);
        self.last = Some(InvocationMetrics {
            fuel_consumed: self.metered.then_some(consumed),
            wall_time: start.elapsed(),
        });
        if let Some(store) = self.store.as_mut() {
            store.data_mut().deadline = None;
            for cap in &self.caps {
                if let Some(s) = store.data_mut().cap_state_dyn(&cap.id()) {
                    cap.end_invocation(s);
                }
            }
        }
        if count {
            self.invocations += 1;
        }
        match outcome {
            Err(ExecError::Exit(0)) => {
                // A clean `proc_exit`: the program is finished and cannot be re-entered.
                self.exit_code = Some(0);
                self.terminate();
                Ok(())
            }
            Err(ExecError::Exit(code)) => {
                self.exit_code = Some(code);
                let e = ExecError::Exit(code);
                self.fail(e.clone());
                Err(e)
            }
            Ok(()) => {
                let _ = self.state.transition(ok_state);
                Ok(())
            }
            Err(e) => {
                self.fail(e.clone());
                Err(e)
            }
        }
    }
}

/// Sets fuel (only on a metered engine) and the absolute deadline for the next stretch of guest
/// execution.
fn arm(store: &mut Store<HostState>, limits: &Limits, metered: bool) -> wasmtime::Result<()> {
    arm_with(store, limits.fuel, limits.timeout, metered)
}

fn arm_with(
    store: &mut Store<HostState>,
    fuel: Option<u64>,
    timeout: Option<Duration>,
    metered: bool,
) -> wasmtime::Result<()> {
    if metered {
        store.set_fuel(fuel.unwrap_or(UNMETERED))?;
    }
    store.data_mut().deadline = timeout.map(|t| Instant::now() + t);
    store.set_epoch_deadline(1);
    Ok(())
}

impl std::fmt::Debug for Isolate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Isolate")
            .field("id", &self.id)
            .field("tenant", &self.tenant)
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}
