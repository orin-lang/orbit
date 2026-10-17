use std::time::Duration;

/// Resource limits of one isolate.
///
/// `fuel` and `timeout` apply to **each** invocation (including initialisation); the
/// memory and table limits apply to the isolate for its whole life.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Maximum linear memory in bytes. Instantiation fails if the module's initial memory is
    /// larger; `memory.grow` beyond it returns `-1` inside the guest.
    pub max_memory_bytes: u64,
    /// Maximum number of elements of any table.
    pub max_table_elements: u32,
    /// Fuel per invocation; `None` means unmetered (fuel is still *measured*).
    pub fuel: Option<u64>,
    /// Wall-clock budget per invocation; `None` means no deadline. Enforced with epoch
    /// interruption, so its resolution is the engine's tick (10 ms by default).
    pub timeout: Option<Duration>,
    /// Byte budget of the log capability per invocation (see `orbit:log/v1`).
    pub max_log_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_memory_bytes: 16 * 1024 * 1024,
            max_table_elements: 10_000,
            fuel: None,
            timeout: None,
            max_log_bytes: orbit_abi::limits::LOG_DEFAULT_BUDGET_BYTES,
        }
    }
}

impl Limits {
    /// Sets [`Limits::max_memory_bytes`].
    #[must_use]
    pub fn with_max_memory(mut self, bytes: u64) -> Self {
        self.max_memory_bytes = bytes;
        self
    }

    /// Sets [`Limits::fuel`].
    #[must_use]
    pub fn with_fuel(mut self, fuel: u64) -> Self {
        self.fuel = Some(fuel);
        self
    }

    /// Sets [`Limits::timeout`].
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Sets [`Limits::max_log_bytes`].
    #[must_use]
    pub fn with_max_log_bytes(mut self, bytes: u64) -> Self {
        self.max_log_bytes = bytes;
        self
    }
}
