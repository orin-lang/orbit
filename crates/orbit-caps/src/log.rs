//! `orbit:log/v1`: `write(level: i32, ptr: i64, len: i64) -> i32`.

use std::any::Any;
use std::fmt;
use std::sync::{Arc, Mutex};

use orbit_abi::{CapabilityId, limits::LOG_MAX_MESSAGE_BYTES, status};
use orbit_runtime::{Capability, HostState, InstanceId, InstanceInfo, TenantId, read_guest_bytes};
use wasmtime::{Caller, Linker};

/// Severity of a log record (`level` argument of `write`: 0 to 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LogLevel {
    /// 0
    Trace,
    /// 1
    Debug,
    /// 2
    Info,
    /// 3
    Warn,
    /// 4
    Error,
}

impl LogLevel {
    /// Converts the ABI value; `None` for anything outside 0..=4.
    #[must_use]
    pub fn from_abi(level: i32) -> Option<Self> {
        Some(match level {
            0 => Self::Trace,
            1 => Self::Debug,
            2 => Self::Info,
            3 => Self::Warn,
            4 => Self::Error,
            _ => return None,
        })
    }

    /// Lower-case name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One accepted log message. `instance` and `tenant` are attached by the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogRecord {
    /// The isolate that logged.
    pub instance: InstanceId,
    /// Its tenant.
    pub tenant: TenantId,
    /// Severity.
    pub level: LogLevel,
    /// The message (valid UTF-8, at most 16 KiB).
    pub message: String,
}

/// Destination of log records. Implementations must be cheap and non-blocking: they run on
/// the executor thread that is driving the guest.
pub trait LogSink: Send + Sync + 'static {
    /// Receives one record.
    fn write(&self, record: &LogRecord);
}

/// Writes `[level] tenant/instance: message` lines to standard error.
#[derive(Clone, Copy, Debug, Default)]
pub struct StderrSink;

impl LogSink for StderrSink {
    fn write(&self, r: &LogRecord) {
        eprintln!("[{}] {}/{}: {}", r.level, r.tenant, r.instance, r.message);
    }
}

/// Collects records in memory; clones share the same buffer. Intended for tests.
#[derive(Clone, Debug, Default)]
pub struct MemorySink {
    records: Arc<Mutex<Vec<LogRecord>>>,
}

impl MemorySink {
    /// An empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A copy of all records received so far, in order.
    #[must_use]
    pub fn records(&self) -> Vec<LogRecord> {
        self.lock().clone()
    }

    /// Removes and returns all records.
    pub fn take(&self) -> Vec<LogRecord> {
        std::mem::take(&mut *self.lock())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<LogRecord>> {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl LogSink for MemorySink {
    fn write(&self, record: &LogRecord) {
        self.lock().push(record.clone());
    }
}

/// Per-instance state: bytes accepted in the current invocation against the budget.
#[derive(Debug)]
struct LogState {
    used: u64,
    budget: u64,
}

/// The `orbit:log/v1` capability.
#[derive(Clone)]
pub struct LogCapability {
    sink: Arc<dyn LogSink>,
}

impl LogCapability {
    /// A log capability that delivers records to `sink`.
    #[must_use]
    pub fn new(sink: impl LogSink) -> Self {
        Self {
            sink: Arc::new(sink),
        }
    }

    /// Like [`new`](Self::new) for an already shared sink.
    #[must_use]
    pub fn with_shared_sink(sink: Arc<dyn LogSink>) -> Self {
        Self { sink }
    }
}

impl fmt::Debug for LogCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LogCapability").finish_non_exhaustive()
    }
}

fn write(
    sink: &dyn LogSink,
    id: &CapabilityId,
    caller: &mut Caller<'_, HostState>,
    level: i32,
    ptr: i64,
    len: i64,
) -> i32 {
    caller.data_mut().record_call(id);
    let Some(level) = LogLevel::from_abi(level) else {
        return status::ERR_INVALID;
    };
    let bytes = match read_guest_bytes(caller, ptr, len, LOG_MAX_MESSAGE_BYTES) {
        Ok(b) => b,
        Err(code) => return code,
    };
    let Ok(message) = String::from_utf8(bytes) else {
        return status::ERR_INVALID;
    };
    let n = message.len() as u64;
    let host = caller.data_mut();
    let Some(state) = host.cap_state_mut::<LogState>(id) else {
        return status::ERR_IO;
    };
    if state.used.saturating_add(n) > state.budget {
        return status::ERR_QUOTA;
    }
    state.used += n;
    let record = LogRecord {
        instance: host.instance_id(),
        tenant: host.tenant().clone(),
        level,
        message,
    };
    host.add_bytes_logged(n);
    sink.write(&record);
    status::OK
}

impl Capability for LogCapability {
    fn id(&self) -> CapabilityId {
        CapabilityId::log_v1()
    }

    fn link(&self, linker: &mut Linker<HostState>) -> wasmtime::Result<()> {
        let id = self.id();
        let module = id.import_module();
        let sink = Arc::clone(&self.sink);
        linker.func_wrap(
            &module,
            "write",
            move |mut caller: Caller<'_, HostState>, level: i32, ptr: i64, len: i64| -> i32 {
                write(&*sink, &id, &mut caller, level, ptr, len)
            },
        )?;
        Ok(())
    }

    fn instance_state(
        &self,
        info: &InstanceInfo,
        _config: &serde_json::Value,
    ) -> Box<dyn Any + Send> {
        Box::new(LogState {
            used: 0,
            budget: info.max_log_bytes,
        })
    }

    fn begin_invocation(&self, state: &mut (dyn Any + Send)) {
        if let Some(s) = state.downcast_mut::<LogState>() {
            s.used = 0;
        }
    }
}
