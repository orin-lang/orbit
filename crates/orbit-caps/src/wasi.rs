//! `wasi_p1/v1`: a restricted WASI preview 1 shim (import module `wasi_snapshot_preview1`).
//!
//! It lets modules built by standard toolchains (clang with wasi-libc, Rust's `wasm32-wasip1`)
//! run on Orbit **without ambient authority**:
//!
//! * `fd_write` on fd 1 / fd 2 goes to the host's [`LogSink`] (info / warn level), split into
//!   lines per instance and flushed when the invocation ends; every other fd is `EBADF`.
//!   Output counts against the instance's per-invocation log budget (`Limits::max_log_bytes`).
//! * stdin is empty: `fd_read` on fd 0 returns 0 bytes (EOF).
//! * there are no preopened directories (`fd_prestat_get` is `EBADF`) and no path functions
//!   exist in the import table at all, so a module that opens files fails to load.
//! * `args_*` / `environ_*` serve only what the grant configures: argv is `["main"]` unless
//!   the grant config has `"args": [...]` (the complete argv, argv[0] included), and the
//!   environment is empty unless the config has `"env": {"K": "V", ...}`. The host's real
//!   arguments and environment are never visible.
//! * `clock_time_get` / `clock_res_get` and `random_get` answer `ENOTCAPABLE` unless the
//!   instance is *also* granted `clock/v1` (resp. `random/v1`). With the clock granted they
//!   use the same time source and coarsening resolution (`resolution_ns` in the clock's
//!   grant config) as `orbit:clock/v1`.
//! * `proc_exit(code)` ends the invocation: the host function returns
//!   [`GuestExit`](orbit_runtime::GuestExit), which the isolate reports as
//!   `ExecError::Exit(code)` (non-zero) or as a clean finish (zero).
//!
//! Every pointer is checked with the runtime's guest-memory helpers; a bad pointer is
//! `EFAULT`, never a host panic. WASI preview 1 is wasm32-only: a module with a 64-bit
//! memory that imports it is rejected at load.

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use orbit_abi::limits::{
    LOG_MAX_MESSAGE_BYTES, WASI_MAX_IOVS, WASI_RANDOM_MAX_BYTES, WASI_WRITE_MAX_BYTES,
};
use orbit_abi::{CapabilityId, status};
use orbit_runtime::{
    Capability, GuestExit, HostState, InstanceId, InstanceInfo, TenantId, read_guest_bytes,
    write_guest_bytes,
};
use wasmtime::{Caller, Linker};

use crate::clock::{
    ClockCapability, SystemTimeSource, TimeSource, coarsen_i, coarsen_u, parse_resolution,
};
use crate::log::{LogLevel, LogRecord, LogSink};

type Errno = u16;

const SUCCESS: Errno = 0;
const BADF: Errno = 8;
const FAULT: Errno = 21;
const INVAL: Errno = 28;
const IO: Errno = 29;
const NOSPC: Errno = 51;
const NOSYS: Errno = 52;
const NOTSUP: Errno = 58;
const SPIPE: Errno = 70;
const NOTCAPABLE: Errno = 76;

const CLOCK_REALTIME: i32 = 0;
const CLOCK_MONOTONIC: i32 = 1;
const CLOCK_PROCESS_CPUTIME: i32 = 2;
const CLOCK_THREAD_CPUTIME: i32 = 3;

const FILETYPE_CHARACTER_DEVICE: u8 = 2;
const RIGHT_FD_READ: u64 = 1 << 1;
const RIGHT_FD_FDSTAT_SET_FLAGS: u64 = 1 << 3;
const RIGHT_FD_WRITE: u64 = 1 << 6;
const RIGHT_FD_FILESTAT_GET: u64 = 1 << 21;
const RIGHT_POLL_FD_READWRITE: u64 = 1 << 27;

/// Grant config used when none is given: `argv = ["main"]`, empty environment.
const DEFAULT_ARGV0: &str = "main";

/// Clock access, present only when the instance is also granted `clock/v1`.
#[derive(Debug, Clone, Copy)]
struct ClockAccess {
    resolution_ns: u64,
    /// Monotonic reading of the source when the instance was created.
    base: u64,
}

/// Per-instance state.
struct WasiState {
    instance: InstanceId,
    tenant: TenantId,
    argv: Vec<Vec<u8>>,
    envp: Vec<Vec<u8>>,
    clock: Option<ClockAccess>,
    random: bool,
    budget: u64,
    used: u64,
    /// Whether the guest closed fd 0, 1, 2.
    closed: [bool; 3],
    /// Output of fd 1 and fd 2 that has not yet ended in a newline.
    pending: [Vec<u8>; 2],
}

impl WasiState {
    fn level(idx: usize) -> LogLevel {
        if idx == 1 {
            LogLevel::Warn
        } else {
            LogLevel::Info
        }
    }

    fn record(&self, sink: &dyn LogSink, idx: usize, bytes: &[u8]) {
        sink.write(&LogRecord {
            instance: self.instance,
            tenant: self.tenant.clone(),
            level: Self::level(idx),
            message: String::from_utf8_lossy(bytes).into_owned(),
        });
    }

    /// Emits `bytes` as one record, or several when it exceeds the 16 KiB message cap.
    fn emit(&self, sink: &dyn LogSink, idx: usize, mut bytes: &[u8]) {
        while bytes.len() > LOG_MAX_MESSAGE_BYTES as usize {
            let cut = chunk_len(bytes);
            self.record(sink, idx, &bytes[..cut]);
            bytes = &bytes[cut..];
        }
        self.record(sink, idx, bytes);
    }

    /// Appends output of stream `idx`, emitting every completed line.
    fn push(&mut self, sink: &dyn LogSink, idx: usize, data: &[u8]) {
        let mut buf = std::mem::take(&mut self.pending[idx]);
        buf.extend_from_slice(data);
        let mut start = 0;
        while let Some(i) = buf[start..].iter().position(|&b| b == b'\n') {
            self.emit(sink, idx, &buf[start..start + i]);
            start += i + 1;
        }
        // An unterminated line is capped at the message size.
        while buf.len() - start >= LOG_MAX_MESSAGE_BYTES as usize {
            let cut = chunk_len(&buf[start..]);
            self.record(sink, idx, &buf[start..start + cut]);
            start += cut;
        }
        buf.drain(..start);
        self.pending[idx] = buf;
    }

    fn flush(&mut self, sink: &dyn LogSink, idx: usize) {
        let buf = std::mem::take(&mut self.pending[idx]);
        if !buf.is_empty() {
            self.emit(sink, idx, &buf);
        }
    }
}

/// Length of the next record cut from `bytes`: at most the message cap, not inside a UTF-8
/// sequence when it can be avoided.
fn chunk_len(bytes: &[u8]) -> usize {
    let max = LOG_MAX_MESSAGE_BYTES as usize;
    let mut cut = bytes.len().min(max);
    if cut < bytes.len() {
        while cut > 0 && (bytes[cut] & 0xC0) == 0x80 {
            cut -= 1;
        }
        if cut == 0 {
            cut = max;
        }
    }
    cut
}

/// The `wasi_p1/v1` capability.
#[derive(Clone)]
pub struct WasiCapability {
    sink: Arc<dyn LogSink>,
    source: Arc<dyn TimeSource>,
}

impl WasiCapability {
    /// A WASI shim whose stdout/stderr go to `sink`, reading the operating system's clock
    /// when an instance is also granted `clock/v1`.
    #[must_use]
    pub fn new(sink: impl LogSink) -> Self {
        Self::with_shared_sink(Arc::new(sink))
    }

    /// Like [`new`](Self::new) for an already shared sink.
    #[must_use]
    pub fn with_shared_sink(sink: Arc<dyn LogSink>) -> Self {
        Self {
            sink,
            source: Arc::new(SystemTimeSource::new()),
        }
    }

    /// Reads time from `source` instead of the system clock.
    #[must_use]
    pub fn with_time_source(mut self, source: Arc<dyn TimeSource>) -> Self {
        self.source = source;
        self
    }

    /// Reads time from the same source as `clock`, so WASI clocks and `orbit:clock/v1` agree.
    #[must_use]
    pub fn with_clock(self, clock: &ClockCapability) -> Self {
        self.with_time_source(clock.time_source())
    }
}

impl fmt::Debug for WasiCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasiCapability").finish_non_exhaustive()
    }
}

#[derive(Clone)]
struct Ctx {
    id: CapabilityId,
    sink: Arc<dyn LogSink>,
    source: Arc<dyn TimeSource>,
}

// --- guest memory ----------------------------------------------------------------------

/// A 32-bit guest address as the `i64` the runtime helpers take (zero-extended).
fn addr(v: i32) -> i64 {
    i64::from(v as u32)
}

fn mem_err(code: i32) -> Errno {
    if code == status::ERR_TOO_LARGE {
        INVAL
    } else {
        FAULT
    }
}

fn get(caller: &mut Caller<'_, HostState>, ptr: i32, len: u32, max: u64) -> Result<Vec<u8>, Errno> {
    read_guest_bytes(caller, addr(ptr), i64::from(len), max).map_err(mem_err)
}

fn put(caller: &mut Caller<'_, HostState>, ptr: i32, bytes: &[u8]) -> Result<(), Errno> {
    write_guest_bytes(caller, addr(ptr), bytes).map_err(mem_err)
}

fn put_u32(caller: &mut Caller<'_, HostState>, ptr: i32, v: u32) -> Result<(), Errno> {
    put(caller, ptr, &v.to_le_bytes())
}

fn put_u64(caller: &mut Caller<'_, HostState>, ptr: i32, v: u64) -> Result<(), Errno> {
    put(caller, ptr, &v.to_le_bytes())
}

fn state<'a>(caller: &'a mut Caller<'_, HostState>, ctx: &Ctx) -> Result<&'a mut WasiState, Errno> {
    caller
        .data_mut()
        .cap_state_mut::<WasiState>(&ctx.id)
        .ok_or(IO)
}

fn errno(r: Result<(), Errno>) -> i32 {
    i32::from(r.err().unwrap_or(SUCCESS))
}

/// Validates a stdio fd (0, 1, 2) that the guest has not closed.
fn stdio(caller: &mut Caller<'_, HostState>, ctx: &Ctx, fd: i32) -> Result<usize, Errno> {
    let idx = usize::try_from(fd).ok().filter(|i| *i < 3).ok_or(BADF)?;
    if state(caller, ctx)?.closed[idx] {
        return Err(BADF);
    }
    Ok(idx)
}

// --- args and environment --------------------------------------------------------------

fn strings(
    caller: &mut Caller<'_, HostState>,
    ctx: &Ctx,
    env: bool,
) -> Result<Vec<Vec<u8>>, Errno> {
    let st = state(caller, ctx)?;
    Ok(if env {
        st.envp.clone()
    } else {
        st.argv.clone()
    })
}

fn sizes_get(
    caller: &mut Caller<'_, HostState>,
    ctx: &Ctx,
    env: bool,
    count_ptr: i32,
    size_ptr: i32,
) -> Result<(), Errno> {
    let list = strings(caller, ctx, env)?;
    let total: usize = list.iter().map(|s| s.len() + 1).sum();
    let (Ok(count), Ok(total)) = (u32::try_from(list.len()), u32::try_from(total)) else {
        return Err(INVAL);
    };
    put_u32(caller, count_ptr, count)?;
    put_u32(caller, size_ptr, total)
}

fn strings_get(
    caller: &mut Caller<'_, HostState>,
    ctx: &Ctx,
    env: bool,
    ptrs: i32,
    buf: i32,
) -> Result<(), Errno> {
    let list = strings(caller, ctx, env)?;
    let mut blob = Vec::new();
    let mut table = Vec::with_capacity(list.len() * 4);
    for s in &list {
        let off = u32::try_from(blob.len()).map_err(|_| INVAL)?;
        let at = (buf as u32).checked_add(off).ok_or(FAULT)?;
        table.extend_from_slice(&at.to_le_bytes());
        blob.extend_from_slice(s);
        blob.push(0);
    }
    put(caller, buf, &blob)?;
    put(caller, ptrs, &table)
}

// --- fds ---------------------------------------------------------------------------------

fn fd_write(
    ctx: &Ctx,
    caller: &mut Caller<'_, HostState>,
    fd: i32,
    iovs: i32,
    iovs_len: i32,
    nwritten: i32,
) -> Result<(), Errno> {
    let idx = stdio(caller, ctx, fd)?;
    if idx == 0 {
        return Err(BADF);
    }
    let idx = idx - 1;
    let data = gather(caller, iovs, iovs_len)?;
    let accept = {
        let st = state(caller, ctx)?;
        let remaining = st.budget.saturating_sub(st.used);
        if !data.is_empty() && remaining == 0 {
            return Err(NOSPC);
        }
        data.len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX))
    };
    // `accept <= WASI_WRITE_MAX_BYTES`, so it fits in u32. Report before committing so a bad
    // `nwritten` pointer leaves no output behind.
    put_u32(caller, nwritten, accept as u32)?;
    let st = state(caller, ctx)?;
    st.used += accept as u64;
    st.push(&*ctx.sink, idx, &data[..accept]);
    caller.data_mut().add_bytes_logged(accept as u64);
    Ok(())
}

/// Collects the bytes of an iovec array, up to `WASI_WRITE_MAX_BYTES` (a longer write is a
/// partial write, which WASI allows).
fn gather(caller: &mut Caller<'_, HostState>, iovs: i32, iovs_len: i32) -> Result<Vec<u8>, Errno> {
    let count = iovs_len as u32;
    if count > WASI_MAX_IOVS {
        return Err(INVAL);
    }
    let table = get(caller, iovs, count * 8, u64::from(WASI_MAX_IOVS) * 8)?;
    let mut data: Vec<u8> = Vec::new();
    for iov in table.chunks_exact(8) {
        let ptr = u32::from_le_bytes([iov[0], iov[1], iov[2], iov[3]]);
        let len = u32::from_le_bytes([iov[4], iov[5], iov[6], iov[7]]);
        let room = WASI_WRITE_MAX_BYTES - data.len() as u64;
        let take = u64::from(len).min(room);
        if take > 0 {
            data.extend(get(caller, ptr as i32, take as u32, WASI_WRITE_MAX_BYTES)?);
        }
        if data.len() as u64 >= WASI_WRITE_MAX_BYTES {
            break;
        }
    }
    Ok(data)
}

fn fd_read(
    ctx: &Ctx,
    caller: &mut Caller<'_, HostState>,
    fd: i32,
    iovs: i32,
    iovs_len: i32,
    nread: i32,
) -> Result<(), Errno> {
    if stdio(caller, ctx, fd)? != 0 {
        return Err(BADF);
    }
    let count = iovs_len as u32;
    if count > WASI_MAX_IOVS {
        return Err(INVAL);
    }
    get(caller, iovs, count * 8, u64::from(WASI_MAX_IOVS) * 8)?;
    put_u32(caller, nread, 0)
}

fn fd_close(ctx: &Ctx, caller: &mut Caller<'_, HostState>, fd: i32) -> Result<(), Errno> {
    let idx = stdio(caller, ctx, fd)?;
    let st = state(caller, ctx)?;
    st.closed[idx] = true;
    if idx > 0 {
        st.flush(&*ctx.sink, idx - 1);
    }
    Ok(())
}

fn rights(idx: usize) -> u64 {
    let io = if idx == 0 {
        RIGHT_FD_READ
    } else {
        RIGHT_FD_WRITE
    };
    io | RIGHT_FD_FDSTAT_SET_FLAGS | RIGHT_FD_FILESTAT_GET | RIGHT_POLL_FD_READWRITE
}

fn fd_fdstat_get(
    ctx: &Ctx,
    caller: &mut Caller<'_, HostState>,
    fd: i32,
    out: i32,
) -> Result<(), Errno> {
    let idx = stdio(caller, ctx, fd)?;
    let mut b = [0u8; 24];
    b[0] = FILETYPE_CHARACTER_DEVICE;
    b[8..16].copy_from_slice(&rights(idx).to_le_bytes());
    put(caller, out, &b)
}

fn fd_filestat_get(
    ctx: &Ctx,
    caller: &mut Caller<'_, HostState>,
    fd: i32,
    out: i32,
) -> Result<(), Errno> {
    stdio(caller, ctx, fd)?;
    let mut b = [0u8; 64];
    b[16] = FILETYPE_CHARACTER_DEVICE;
    put(caller, out, &b)
}

// --- clocks and random -----------------------------------------------------------------

fn clock_access(caller: &mut Caller<'_, HostState>, ctx: &Ctx) -> Result<ClockAccess, Errno> {
    state(caller, ctx)?.clock.ok_or(NOTCAPABLE)
}

fn clock_res_get(
    ctx: &Ctx,
    caller: &mut Caller<'_, HostState>,
    id: i32,
    out: i32,
) -> Result<(), Errno> {
    let clock = clock_access(caller, ctx)?;
    match id {
        CLOCK_REALTIME | CLOCK_MONOTONIC => put_u64(caller, out, clock.resolution_ns),
        CLOCK_PROCESS_CPUTIME | CLOCK_THREAD_CPUTIME => Err(NOTSUP),
        _ => Err(INVAL),
    }
}

fn clock_time_get(
    ctx: &Ctx,
    caller: &mut Caller<'_, HostState>,
    id: i32,
    _precision: i64,
    out: i32,
) -> Result<(), Errno> {
    let clock = clock_access(caller, ctx)?;
    let nanos = match id {
        CLOCK_REALTIME => {
            u64::try_from(coarsen_i(ctx.source.now_unix_nanos(), clock.resolution_ns)).unwrap_or(0)
        }
        CLOCK_MONOTONIC => coarsen_u(
            ctx.source.monotonic_nanos().saturating_sub(clock.base),
            clock.resolution_ns,
        ),
        CLOCK_PROCESS_CPUTIME | CLOCK_THREAD_CPUTIME => return Err(NOTSUP),
        _ => return Err(INVAL),
    };
    put_u64(caller, out, nanos)
}

fn random_get(
    ctx: &Ctx,
    caller: &mut Caller<'_, HostState>,
    buf: i32,
    len: i32,
) -> Result<(), Errno> {
    if !state(caller, ctx)?.random {
        return Err(NOTCAPABLE);
    }
    let len = len as u32;
    if u64::from(len) > WASI_RANDOM_MAX_BYTES {
        return Err(INVAL);
    }
    let mut bytes = vec![0u8; len as usize];
    getrandom::fill(&mut bytes).map_err(|_| IO)?;
    put(caller, buf, &bytes)
}

// --- linking ---------------------------------------------------------------------------

/// Registers one import: counts the call and converts the `Result<(), Errno>` to the i32.
macro_rules! reg {
    ($linker:expr, $module:expr, $base:expr, $name:literal,
     |$caller:ident, $ctx:ident $(, $p:ident : $t:ty)*| $body:expr) => {{
        let $ctx = $base.clone();
        $linker.func_wrap(
            $module,
            $name,
            move |mut raw: Caller<'_, HostState> $(, $p: $t)*| -> i32 {
                let $caller = &mut raw;
                $caller.data_mut().record_call(&$ctx.id);
                let result: Result<(), Errno> = $body;
                errno(result)
            },
        )?;
    }};
}

impl Capability for WasiCapability {
    fn id(&self) -> CapabilityId {
        CapabilityId::wasi_p1_v1()
    }

    fn link(&self, linker: &mut Linker<HostState>) -> wasmtime::Result<()> {
        let id = self.id();
        let module = id.import_module();
        let base = Ctx {
            id,
            sink: Arc::clone(&self.sink),
            source: Arc::clone(&self.source),
        };
        let m = module.as_str();

        reg!(linker, m, base, "args_sizes_get", |c, x, a: i32, b: i32| {
            sizes_get(c, &x, false, a, b)
        });
        reg!(linker, m, base, "args_get", |c, x, a: i32, b: i32| {
            strings_get(c, &x, false, a, b)
        });
        reg!(
            linker,
            m,
            base,
            "environ_sizes_get",
            |c, x, a: i32, b: i32| { sizes_get(c, &x, true, a, b) }
        );
        reg!(linker, m, base, "environ_get", |c, x, a: i32, b: i32| {
            strings_get(c, &x, true, a, b)
        });
        reg!(
            linker,
            m,
            base,
            "clock_res_get",
            |c, x, id: i32, out: i32| { clock_res_get(&x, c, id, out) }
        );
        reg!(
            linker,
            m,
            base,
            "clock_time_get",
            |c, x, id: i32, precision: i64, out: i32| { clock_time_get(&x, c, id, precision, out) }
        );
        reg!(linker, m, base, "fd_close", |c, x, fd: i32| fd_close(
            &x, c, fd
        ));
        reg!(
            linker,
            m,
            base,
            "fd_fdstat_get",
            |c, x, fd: i32, out: i32| { fd_fdstat_get(&x, c, fd, out) }
        );
        reg!(
            linker,
            m,
            base,
            "fd_fdstat_set_flags",
            |c, x, fd: i32, _flags: i32| { stdio(c, &x, fd).map(|_| ()) }
        );
        reg!(
            linker,
            m,
            base,
            "fd_filestat_get",
            |c, x, fd: i32, out: i32| { fd_filestat_get(&x, c, fd, out) }
        );
        // No preopened directories: the first probe ends wasi-libc's / Rust's scan.
        reg!(
            linker,
            m,
            base,
            "fd_prestat_get",
            |_c, _x, _fd: i32, _out: i32| Err(BADF)
        );
        reg!(
            linker,
            m,
            base,
            "fd_prestat_dir_name",
            |_c, _x, _fd: i32, _p: i32, _l: i32| { Err(BADF) }
        );
        reg!(
            linker,
            m,
            base,
            "fd_read",
            |c, x, fd: i32, iovs: i32, n: i32, out: i32| { fd_read(&x, c, fd, iovs, n, out) }
        );
        reg!(
            linker,
            m,
            base,
            "fd_seek",
            |c, x, fd: i32, _off: i64, _whence: i32, _out: i32| {
                // Stdio are character devices: not seekable.
                stdio(c, &x, fd).and(Err(SPIPE))
            }
        );
        reg!(
            linker,
            m,
            base,
            "fd_write",
            |c, x, fd: i32, iovs: i32, n: i32, out: i32| { fd_write(&x, c, fd, iovs, n, out) }
        );
        reg!(
            linker,
            m,
            base,
            "poll_oneoff",
            |_c, _x, _i: i32, _o: i32, _n: i32, _e: i32| Err(NOSYS)
        );
        reg!(linker, m, base, "random_get", |c, x, buf: i32, len: i32| {
            random_get(&x, c, buf, len)
        });
        reg!(linker, m, base, "sched_yield", |_c, _x| Ok(()));

        let exit_ctx = base;
        linker.func_wrap(
            m,
            "proc_exit",
            move |mut caller: Caller<'_, HostState>, code: i32| -> wasmtime::Result<()> {
                caller.data_mut().record_call(&exit_ctx.id);
                Err(wasmtime::Error::new(GuestExit(code)))
            },
        )?;
        Ok(())
    }

    fn instance_state(
        &self,
        info: &InstanceInfo,
        config: &serde_json::Value,
    ) -> Box<dyn Any + Send> {
        let argv = match config.get("args").and_then(serde_json::Value::as_array) {
            Some(list) => list
                .iter()
                .filter_map(serde_json::Value::as_str)
                .filter(|s| !s.contains('\0'))
                .map(|s| s.as_bytes().to_vec())
                .collect(),
            None => vec![DEFAULT_ARGV0.as_bytes().to_vec()],
        };
        let envp = config
            .get("env")
            .and_then(serde_json::Value::as_object)
            .map(|vars| {
                vars.iter()
                    .filter_map(|(k, v)| Some((k, v.as_str()?)))
                    .filter(|(k, v)| !k.is_empty() && !k.contains(['=', '\0']) && !v.contains('\0'))
                    .map(|(k, v)| format!("{k}={v}").into_bytes())
                    .collect()
            })
            .unwrap_or_default();
        let clock = info
            .grants
            .config(&CapabilityId::clock_v1())
            .map(|cfg| ClockAccess {
                resolution_ns: parse_resolution(cfg),
                base: self.source.monotonic_nanos(),
            });
        Box::new(WasiState {
            instance: info.instance_id,
            tenant: info.tenant.clone(),
            argv,
            envp,
            clock,
            random: info.grants.contains(&CapabilityId::random_v1()),
            budget: info.max_log_bytes,
            used: 0,
            closed: [false; 3],
            pending: [Vec::new(), Vec::new()],
        })
    }

    fn begin_invocation(&self, state: &mut (dyn Any + Send)) {
        if let Some(s) = state.downcast_mut::<WasiState>() {
            s.used = 0;
        }
    }

    fn end_invocation(&self, state: &mut (dyn Any + Send)) {
        if let Some(s) = state.downcast_mut::<WasiState>() {
            for idx in 0..2 {
                s.flush(&*self.sink, idx);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunking_respects_utf8() {
        let max = LOG_MAX_MESSAGE_BYTES as usize;
        let mut v = vec![b'a'; max - 1];
        v.extend("é".as_bytes()); // straddles the cap
        v.extend([b'b'; 10]);
        let cut = chunk_len(&v);
        assert_eq!(cut, max - 1);
        assert!(std::str::from_utf8(&v[..cut]).is_ok());
        assert_eq!(chunk_len(b"short"), 5);
    }
}
