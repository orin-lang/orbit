//! `orbit:process/v1`: explicit arguments, environment and standard input.
//!
//! Nothing is inherited from the host. The grant config is
//! `{"args":["main","a"],"env":{"K":"V"},"stdin":false}`; `stdin` is live only when it is
//! `true` **and** the capability was built with a [`StdinProvider`] (the CLI builds one over the
//! real standard input). Without the stdin grant `stdin_read` returns `ERR_DENIED`.

use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::sync::mpsc::{Receiver, RecvTimeoutError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use orbit_abi::{CapabilityId, limits::FS_MAX_IO_BYTES, status};
use orbit_runtime::{Capability, HostState, InstanceInfo, read_guest_bytes, write_guest_bytes};
use serde_json::{Value, json};
use wasmtime::{Caller, Linker};

/// Longest environment variable name accepted by `env_get`.
pub const ENV_MAX_NAME_BYTES: u64 = 1024;

/// A stdin read failed.
#[derive(Debug, PartialEq, Eq)]
pub enum StdinError {
    /// No data arrived within the timeout.
    Timeout,
    /// The source failed.
    Io,
}

/// One instance's view of standard input.
pub trait StdinReader: Send {
    /// Reads up to `buf.len()` bytes; `Ok(0)` is end of input. `timeout` of `None` waits
    /// without bound, `Some(0)` polls.
    ///
    /// # Errors
    /// [`StdinError::Timeout`] or [`StdinError::Io`].
    fn read(&mut self, buf: &mut [u8], timeout: Option<Duration>) -> Result<usize, StdinError>;
}

/// Creates the stdin of each instance that is granted it.
pub trait StdinProvider: Send + Sync + 'static {
    /// A reader for the new instance.
    fn open(&self, info: &InstanceInfo) -> Box<dyn StdinReader>;
}

/// Fixed bytes as stdin; every instance gets its own copy from the start.
#[derive(Clone, Debug, Default)]
pub struct MemoryStdin(pub Vec<u8>);

struct MemReader(std::io::Cursor<Vec<u8>>);

impl StdinReader for MemReader {
    fn read(&mut self, buf: &mut [u8], _t: Option<Duration>) -> Result<usize, StdinError> {
        self.0.read(buf).map_err(|_| StdinError::Io)
    }
}

impl StdinProvider for MemoryStdin {
    fn open(&self, _info: &InstanceInfo) -> Box<dyn StdinReader> {
        Box::new(MemReader(std::io::Cursor::new(self.0.clone())))
    }
}

struct Pump {
    rx: Receiver<Option<Vec<u8>>>,
    left: Vec<u8>,
    pos: usize,
    eof: bool,
}

/// The host process's real standard input. A helper thread (started on first use) reads it so
/// that reads can time out. It is one shared stream: instances granted stdin share it.
#[derive(Default)]
pub struct HostStdin {
    pump: Arc<OnceLock<Arc<Mutex<Pump>>>>,
}

impl HostStdin {
    /// The process standard input.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

struct HostReader(Arc<Mutex<Pump>>);

impl StdinReader for HostReader {
    fn read(&mut self, buf: &mut [u8], timeout: Option<Duration>) -> Result<usize, StdinError> {
        let mut p = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if p.pos >= p.left.len() {
            if p.eof {
                return Ok(0);
            }
            let msg = match timeout {
                None => p.rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                Some(t) => p.rx.recv_timeout(t),
            };
            match msg {
                Ok(Some(chunk)) => {
                    p.left = chunk;
                    p.pos = 0;
                }
                Ok(None) | Err(RecvTimeoutError::Disconnected) => {
                    p.eof = true;
                    return Ok(0);
                }
                Err(RecvTimeoutError::Timeout) => return Err(StdinError::Timeout),
            }
        }
        let n = buf.len().min(p.left.len() - p.pos);
        buf[..n].copy_from_slice(&p.left[p.pos..p.pos + n]);
        p.pos += n;
        Ok(n)
    }
}

impl StdinProvider for HostStdin {
    fn open(&self, _info: &InstanceInfo) -> Box<dyn StdinReader> {
        let pump = self.pump.get_or_init(|| {
            let (tx, rx) = sync_channel::<Option<Vec<u8>>>(4);
            std::thread::spawn(move || {
                let mut stdin = std::io::stdin().lock();
                let mut buf = vec![0u8; 8192];
                loop {
                    match stdin.read(&mut buf) {
                        Ok(0) | Err(_) => {
                            let _ = tx.send(None);
                            return;
                        }
                        Ok(n) => {
                            if tx.send(Some(buf[..n].to_vec())).is_err() {
                                return;
                            }
                        }
                    }
                }
            });
            Arc::new(Mutex::new(Pump {
                rx,
                left: Vec::new(),
                pos: 0,
                eof: false,
            }))
        });
        Box::new(HostReader(Arc::clone(pump)))
    }
}

/// Builder for the grant configuration of `process/v1`.
#[derive(Clone, Debug, Default)]
pub struct ProcessGrant {
    args: Vec<String>,
    env: BTreeMap<String, String>,
    stdin: bool,
}

impl ProcessGrant {
    /// No arguments, no environment, no stdin.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    /// Sets the full argument vector (`argv[0]` is the first element).
    #[must_use]
    pub fn args<S: Into<String>>(mut self, args: impl IntoIterator<Item = S>) -> Self {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }
    /// Defines one environment variable.
    #[must_use]
    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.insert(k.into(), v.into());
        self
    }
    /// Allows `stdin_read`.
    #[must_use]
    pub fn stdin(mut self, on: bool) -> Self {
        self.stdin = on;
        self
    }
    /// The JSON configuration.
    #[must_use]
    pub fn to_config(&self) -> Value {
        json!({"args": self.args, "env": self.env, "stdin": self.stdin})
    }
}

struct ProcState {
    args: Vec<String>,
    env: BTreeMap<String, String>,
    stdin: Option<Box<dyn StdinReader>>,
}

fn state<'a>(host: &'a mut HostState, id: &CapabilityId) -> Option<&'a mut ProcState> {
    host.cap_state_mut::<ProcState>(id)
}

fn put(caller: &mut Caller<'_, HostState>, s: &str, ptr: i64, cap: i64) -> i64 {
    if cap < 0 {
        return status::ERR_INVALID.into();
    }
    if s.len() as u64 > cap as u64 {
        return status::ERR_TOO_LARGE.into();
    }
    match write_guest_bytes(caller, ptr, s.as_bytes()) {
        Ok(()) => i64::try_from(s.len()).unwrap_or(i64::MAX),
        Err(c) => c.into(),
    }
}

/// The `orbit:process/v1` capability.
#[derive(Clone, Default)]
pub struct ProcessCapability {
    stdin: Option<Arc<dyn StdinProvider>>,
}

impl fmt::Debug for ProcessCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcessCapability").finish_non_exhaustive()
    }
}

impl ProcessCapability {
    /// Arguments and environment only; `stdin_read` is always `ERR_DENIED`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Also serves stdin to instances whose grant sets `stdin`.
    #[must_use]
    pub fn with_stdin(mut self, provider: impl StdinProvider) -> Self {
        self.stdin = Some(Arc::new(provider));
        self
    }
}

impl Capability for ProcessCapability {
    fn id(&self) -> CapabilityId {
        CapabilityId::process_v1()
    }

    fn link(&self, linker: &mut Linker<HostState>) -> wasmtime::Result<()> {
        let id = self.id();
        let m = id.import_module();
        let i = id.clone();
        linker.func_wrap(
            &m,
            "arg_count",
            move |mut c: Caller<'_, HostState>| -> i32 {
                c.data_mut().record_call(&i);
                state(c.data_mut(), &i)
                    .map_or(0, |s| i32::try_from(s.args.len()).unwrap_or(i32::MAX))
            },
        )?;
        let i = id.clone();
        linker.func_wrap(
            &m,
            "arg",
            move |mut c: Caller<'_, HostState>, n: i32, ptr: i64, cap: i64| -> i64 {
                c.data_mut().record_call(&i);
                let Some(a) = state(c.data_mut(), &i)
                    .and_then(|s| usize::try_from(n).ok().and_then(|n| s.args.get(n).cloned()))
                else {
                    return status::ERR_INVALID.into();
                };
                put(&mut c, &a, ptr, cap)
            },
        )?;
        let i = id.clone();
        linker.func_wrap(
            &m,
            "env_get",
            move |mut c: Caller<'_, HostState>, np: i64, nl: i64, ptr: i64, cap: i64| -> i64 {
                c.data_mut().record_call(&i);
                let name = match read_guest_bytes(&mut c, np, nl, ENV_MAX_NAME_BYTES) {
                    Ok(b) => b,
                    Err(e) => return e.into(),
                };
                let Ok(name) = String::from_utf8(name) else {
                    return status::ERR_INVALID.into();
                };
                if name.is_empty() || name.contains(['=', '\0']) {
                    return status::ERR_INVALID.into();
                }
                let Some(v) = state(c.data_mut(), &i).and_then(|s| s.env.get(&name).cloned())
                else {
                    return status::ERR_NOT_FOUND.into();
                };
                put(&mut c, &v, ptr, cap)
            },
        )?;
        let i = id;
        linker.func_wrap(
            &m,
            "stdin_read",
            move |mut c: Caller<'_, HostState>, ptr: i64, cap: i64, timeout_ms: i64| -> i64 {
                c.data_mut().record_call(&i);
                if cap < 0 {
                    return status::ERR_INVALID.into();
                }
                let n = (cap as u64).min(FS_MAX_IO_BYTES) as usize;
                let timeout = u64::try_from(timeout_ms).ok().map(Duration::from_millis);
                if let Err(e) = crate::fs::check_out(&mut c, ptr, n as i64) {
                    return e.into();
                }
                let mut buf = vec![0u8; n];
                let Some(s) = state(c.data_mut(), &i) else {
                    return status::ERR_IO.into();
                };
                let Some(r) = s.stdin.as_mut() else {
                    return status::ERR_DENIED.into();
                };
                // Validate the destination before consuming input.
                let got = match r.read(&mut buf, timeout) {
                    Ok(g) => g,
                    Err(StdinError::Timeout) => return status::ERR_TIMEOUT.into(),
                    Err(StdinError::Io) => return status::ERR_IO.into(),
                };
                match write_guest_bytes(&mut c, ptr, &buf[..got]) {
                    Ok(()) => i64::try_from(got).unwrap_or(i64::MAX),
                    Err(e) => e.into(),
                }
            },
        )?;
        Ok(())
    }

    fn instance_state(
        &self,
        info: &InstanceInfo,
        config: &serde_json::Value,
    ) -> Box<dyn Any + Send> {
        let args = config
            .get("args")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let env = config
            .get("env")
            .and_then(Value::as_object)
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        let want_stdin = config.get("stdin").and_then(Value::as_bool) == Some(true);
        let stdin = self
            .stdin
            .as_ref()
            .filter(|_| want_stdin)
            .map(|p| p.open(info));
        Box::new(ProcState { args, env, stdin })
    }
}
