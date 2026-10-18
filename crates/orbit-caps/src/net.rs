//! `orbit:net/v1`: TCP networking, an image of the Orin L0 platform layer (ADR-0010).
//!
//! The grant is the authority. Its configuration is an object, all keys optional:
//!
//! ```json
//! {"listen": ["127.0.0.1:8080", "*:9000-9010"], "connect": ["*.example.com:443"],
//!  "max_sockets": 64, "max_bytes_per_call": 1048576}
//! ```
//!
//! A scope is `host:port` or `host:lo-hi` (IPv6 literals in brackets). `host` is `*` (any),
//! `*.suffix` (names ending in `.suffix`) or an exact, ASCII-case-insensitive text; for
//! `listen` the text is an address literal. A request outside every scope is refused with
//! `ERR_DENIED` **before any socket exists**. Without a `listen` / `connect` list nothing is
//! allowed.
//!
//! All state of one isolate (socket handles, wake sources, limits) lives in one
//! `Arc<NetShared>`, so a later multi-threaded isolate can share it between wasm instances.
//! Waiting calls are real async host functions: they never block an executor thread, and a
//! wait never runs longer than the isolate's remaining wall-clock deadline.

use std::any::Any;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Shutdown, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use orbit_abi::{CapabilityId, limits as lim, status};
use orbit_runtime::{
    Capability, HandleTable, HostState, InstanceInfo, check_guest_range, read_guest_bytes,
    write_guest_bytes,
};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::Notify;
use wasmtime::{Caller, Linker};

const MAX_NAME_BYTES: u64 = 255;
const DNS_TIMEOUT: Duration = Duration::from_secs(30);

// --- scopes -------------------------------------------------------------------------------

/// One entry of a `listen` or `connect` grant: a host pattern and a port range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetScope {
    host: String,
    lo: u16,
    hi: u16,
}

impl NetScope {
    /// Parses `host:port` or `host:lo-hi`.
    ///
    /// # Errors
    /// A message when the text is malformed (empty host, `*` inside a name, bad ports).
    pub fn parse(text: &str) -> Result<Self, String> {
        let (host, ports) = text
            .rsplit_once(':')
            .ok_or_else(|| format!("`{text}`: expected host:port or host:lo-hi"))?;
        let host = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        if host.is_empty() {
            return Err(format!("`{text}`: empty host"));
        }
        if host != "*" && host.trim_start_matches("*.").contains('*') {
            return Err(format!(
                "`{text}`: `*` only as the whole host or a `*.` prefix"
            ));
        }
        let port = |s: &str| {
            s.parse::<u16>()
                .map_err(|_| format!("`{text}`: bad port `{s}`"))
        };
        let (lo, hi) = match ports.split_once('-') {
            Some((a, b)) => (port(a)?, port(b)?),
            None => {
                let p = port(ports)?;
                (p, p)
            }
        };
        if lo > hi {
            return Err(format!("`{text}`: empty port range"));
        }
        Ok(Self {
            host: host.to_ascii_lowercase(),
            lo,
            hi,
        })
    }

    fn host_matches(&self, name: &str) -> bool {
        if self.host == "*" {
            return true;
        }
        if let Some(sfx) = self.host.strip_prefix('*') {
            // `*.example.com` matches `a.example.com`, not `example.com`.
            return name.len() > sfx.len() && name.to_ascii_lowercase().ends_with(sfx);
        }
        self.host.eq_ignore_ascii_case(name)
    }

    fn covers(&self, name: &str, port: u16) -> bool {
        self.host_matches(name) && (self.lo..=self.hi).contains(&port)
    }
}

/// Builds the grant configuration for `orbit:net/v1` from scope texts.
#[must_use]
pub fn net_grant_config(listen: &[String], connect: &[String]) -> serde_json::Value {
    serde_json::json!({ "listen": listen, "connect": connect })
}

// --- wake sources -------------------------------------------------------------------------

/// A cancellation token. Signalling a node also signals every descendant (the L0 tree: a
/// wait observes its source and all ancestors).
#[derive(Debug, Default)]
struct WakeNode {
    set: AtomicBool,
    notify: Notify,
    children: Mutex<Vec<Weak<WakeNode>>>,
}

impl WakeNode {
    fn is_set(&self) -> bool {
        self.set.load(Ordering::SeqCst)
    }

    fn signal(&self) -> bool {
        if self.set.swap(true, Ordering::SeqCst) {
            return false;
        }
        self.notify.notify_waiters();
        let kids: Vec<_> = lock(&self.children)
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        for k in kids {
            k.signal();
        }
        true
    }

    fn child(self: &Arc<Self>) -> Arc<WakeNode> {
        let mut kids = lock(&self.children);
        kids.retain(|w| w.strong_count() > 0);
        let c = Arc::new(WakeNode::default());
        // Read after taking the lock: a concurrent `signal` either sees `c` in the list or
        // has already set the flag we read here.
        if self.is_set() {
            c.set.store(true, Ordering::SeqCst);
        }
        kids.push(Arc::downgrade(&c));
        c
    }

    async fn cancelled(&self) {
        loop {
            let n = self.notify.notified();
            if self.is_set() {
                return;
            }
            n.await;
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

// --- per-isolate state --------------------------------------------------------------------

struct Conn {
    tcp: TcpStream,
    peer: SocketAddr,
}

enum Sock {
    Listener(Arc<TcpListener>),
    Stream(Arc<Conn>),
}

struct NetShared {
    listen: Vec<NetScope>,
    connect: Vec<NetScope>,
    max_bytes: u64,
    socks: Mutex<HandleTable<Sock>>,
    wakes: Mutex<HandleTable<Arc<WakeNode>>>,
}

struct NetState(Arc<NetShared>);

impl NetShared {
    fn new(info: &InstanceInfo, config: &serde_json::Value) -> Self {
        let scopes = |key: &str| -> Vec<NetScope> {
            config
                .get(key)
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str().and_then(|s| NetScope::parse(s).ok()))
                        .collect()
                })
                .unwrap_or_default()
        };
        let num = |key: &str| config.get(key).and_then(serde_json::Value::as_u64);
        let tag = info.instance_id.0;
        Self {
            listen: scopes("listen"),
            connect: scopes("connect"),
            max_bytes: num("max_bytes_per_call").unwrap_or(lim::NET_DEFAULT_MAX_BYTES_PER_CALL),
            socks: Mutex::new(HandleTable::new(
                tag,
                num("max_sockets").map_or(lim::NET_DEFAULT_MAX_SOCKETS, |n| n as usize),
            )),
            wakes: Mutex::new(HandleTable::new(tag ^ 0x2a5a, lim::NET_MAX_WAKE_SOURCES)),
        }
    }

    fn allowed(scopes: &[NetScope], name: &str, port: u16) -> bool {
        scopes.iter().any(|s| s.covers(name, port))
    }

    fn sock(&self, h: i64) -> Result<SockRef, i32> {
        match lock(&self.socks).get(h) {
            Some(Sock::Listener(l)) => Ok(SockRef::Listener(Arc::clone(l))),
            Some(Sock::Stream(s)) => Ok(SockRef::Stream(Arc::clone(s))),
            None => Err(status::ERR_BAD_HANDLE),
        }
    }

    fn listener(&self, h: i64) -> Result<Arc<TcpListener>, i32> {
        match self.sock(h)? {
            SockRef::Listener(l) => Ok(l),
            SockRef::Stream(_) => Err(status::ERR_BAD_HANDLE),
        }
    }

    fn stream(&self, h: i64) -> Result<Arc<Conn>, i32> {
        match self.sock(h)? {
            SockRef::Stream(s) => Ok(s),
            SockRef::Listener(_) => Err(status::ERR_BAD_HANDLE),
        }
    }

    fn has_room(&self) -> Result<(), i32> {
        // `HandleTable` has no capacity getter: probe through a dry insert is not possible,
        // so the limit is enforced at insert; this early check only avoids waiting for
        // nothing when the table is already full.
        let t = lock(&self.socks);
        if t.is_full() {
            Err(status::ERR_QUOTA)
        } else {
            Ok(())
        }
    }

    fn add(&self, s: Sock) -> i64 {
        lock(&self.socks)
            .insert(s)
            .unwrap_or(i64::from(status::ERR_QUOTA))
    }

    fn wake(&self, h: i64) -> Result<Option<Arc<WakeNode>>, i32> {
        if h == 0 {
            return Ok(None);
        }
        lock(&self.wakes)
            .get(h)
            .map(|n| Some(Arc::clone(n)))
            .ok_or(status::ERR_BAD_HANDLE)
    }
}

enum SockRef {
    Listener(Arc<TcpListener>),
    Stream(Arc<Conn>),
}

fn io_status(e: &io::Error) -> i32 {
    use io::ErrorKind::{
        AddrInUse, BrokenPipe, ConnectionAborted, ConnectionRefused, ConnectionReset, NotConnected,
        TimedOut,
    };
    match e.kind() {
        ConnectionRefused => status::ERR_REFUSED,
        AddrInUse => status::ERR_ADDR_IN_USE,
        ConnectionReset | ConnectionAborted | BrokenPipe | NotConnected => status::ERR_RESET,
        TimedOut => status::ERR_TIMEOUT,
        _ => status::ERR_IO,
    }
}

fn timeout_ok(ms: i32) -> bool {
    (0..=lim::NET_MAX_TIMEOUT_MS).contains(&ms)
}

/// Runs `fut` bounded by `timeout_ms`, the isolate's remaining deadline and the wake source.
/// A wake source that is already set wins before anything starts.
async fn guarded<T>(
    fut: impl Future<Output = Result<T, i32>>,
    timeout_ms: i32,
    remaining: Option<Duration>,
    wake: Option<&Arc<WakeNode>>,
) -> Result<T, i32> {
    if wake.is_some_and(|w| w.is_set()) {
        return Err(status::ERR_CANCELLED);
    }
    let mut limit = Duration::from_millis(timeout_ms as u64);
    if let Some(r) = remaining {
        limit = limit.min(r);
    }
    let cancelled = async {
        match wake {
            Some(w) => w.cancelled().await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        biased;
        () = cancelled => Err(status::ERR_CANCELLED),
        r = tokio::time::timeout(limit, fut) => r.unwrap_or(Err(status::ERR_TIMEOUT)),
    }
}

/// A host-side handle on one isolate's network state. It is `Send + Sync + Clone` and stays
/// usable while the isolate is busy inside a waiting call, which is how a host cancels it.
#[derive(Clone)]
pub struct NetController(Arc<NetShared>);

impl NetController {
    /// The controller of `isolate`, if it was granted `net/v1` and is not terminated.
    pub fn of(isolate: &mut orbit_runtime::Isolate) -> Option<Self> {
        isolate
            .cap_state_mut::<NetState>(&CapabilityId::net_v1())
            .map(|s| Self(Arc::clone(&s.0)))
    }

    /// Signals wake source `handle` (and its descendants). `false` for a stale or unknown
    /// handle.
    #[must_use]
    pub fn signal(&self, handle: i64) -> bool {
        match self.0.wake(handle) {
            Ok(Some(n)) => {
                n.signal();
                true
            }
            _ => false,
        }
    }

    /// Number of open sockets (listeners and streams).
    #[must_use]
    pub fn open_sockets(&self) -> usize {
        lock(&self.0.socks).len()
    }
}

// --- the capability -----------------------------------------------------------------------

/// The `orbit:net/v1` capability.
#[derive(Clone, Copy, Debug, Default)]
pub struct NetCapability;

impl NetCapability {
    /// A network capability; what an isolate may reach is decided by its grant.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

fn shared(id: &CapabilityId, caller: &mut Caller<'_, HostState>) -> Option<Arc<NetShared>> {
    caller.data_mut().record_call(id);
    caller
        .data_mut()
        .cap_state_mut::<NetState>(id)
        .map(|s| Arc::clone(&s.0))
}

fn text(caller: &mut Caller<'_, HostState>, ptr: i64, len: i64) -> Result<String, i32> {
    let bytes = read_guest_bytes(caller, ptr, len, MAX_NAME_BYTES)?;
    let s = String::from_utf8(bytes).map_err(|_| status::ERR_INVALID)?;
    if s.is_empty() || s.contains('*') || s.contains('\0') {
        return Err(status::ERR_INVALID);
    }
    Ok(s)
}

fn code(r: Result<i64, i32>) -> i64 {
    r.unwrap_or_else(i64::from)
}

fn code32(r: Result<i32, i32>) -> i32 {
    r.unwrap_or_else(|c| c)
}

async fn tcp_listen(
    sh: &NetShared,
    caller: &mut Caller<'_, HostState>,
    (ptr, len, port, backlog, opt): (i64, i64, i32, i32, i32),
) -> Result<i64, i32> {
    let addr = text(caller, ptr, len)?;
    let port = u16::try_from(port).map_err(|_| status::ERR_INVALID)?;
    if !(1..=4096).contains(&backlog) {
        return Err(status::ERR_INVALID);
    }
    if !NetShared::allowed(&sh.listen, &addr, port) {
        return Err(status::ERR_DENIED);
    }
    let ip: IpAddr = addr.parse().map_err(|_| status::ERR_INVALID)?;
    sh.has_room()?;
    let sock = if ip.is_ipv4() {
        TcpSocket::new_v4()
    } else {
        TcpSocket::new_v6()
    }
    .map_err(|e| io_status(&e))?;
    if opt & 1 != 0 {
        sock.set_reuseaddr(true).map_err(|e| io_status(&e))?;
    }
    sock.bind(SocketAddr::new(ip, port))
        .map_err(|e| io_status(&e))?;
    let listener = sock.listen(backlog as u32).map_err(|e| io_status(&e))?;
    Ok(sh.add(Sock::Listener(Arc::new(listener))))
}

async fn tcp_accept(
    sh: &NetShared,
    caller: &mut Caller<'_, HostState>,
    (h, timeout_ms, wake): (i64, i32, i64),
) -> Result<i64, i32> {
    let listener = sh.listener(h)?;
    if !timeout_ok(timeout_ms) {
        return Err(status::ERR_INVALID);
    }
    let wake = sh.wake(wake)?;
    sh.has_room()?;
    let remaining = caller.data().deadline_remaining();
    let (tcp, peer) = guarded(
        async { listener.accept().await.map_err(|e| io_status(&e)) },
        timeout_ms,
        remaining,
        wake.as_ref(),
    )
    .await?;
    let _ = tcp.set_nodelay(true);
    Ok(sh.add(Sock::Stream(Arc::new(Conn { tcp, peer }))))
}

async fn tcp_connect(
    sh: &NetShared,
    caller: &mut Caller<'_, HostState>,
    (ptr, len, port, timeout_ms, wake): (i64, i64, i32, i32, i64),
) -> Result<i64, i32> {
    let host = text(caller, ptr, len)?;
    let port = u16::try_from(port)
        .ok()
        .filter(|p| *p != 0)
        .ok_or(status::ERR_INVALID)?;
    if !timeout_ok(timeout_ms) {
        return Err(status::ERR_INVALID);
    }
    if !NetShared::allowed(&sh.connect, &host, port) {
        return Err(status::ERR_DENIED);
    }
    let wake = sh.wake(wake)?;
    sh.has_room()?;
    let remaining = caller.data().deadline_remaining();
    let tcp = guarded(
        async {
            TcpStream::connect((host.as_str(), port))
                .await
                .map_err(|e| io_status(&e))
        },
        timeout_ms,
        remaining,
        wake.as_ref(),
    )
    .await?;
    let _ = tcp.set_nodelay(true);
    let peer = tcp.peer_addr().map_err(|e| io_status(&e))?;
    Ok(sh.add(Sock::Stream(Arc::new(Conn { tcp, peer }))))
}

async fn recv(
    sh: &NetShared,
    caller: &mut Caller<'_, HostState>,
    (h, ptr, cap, timeout_ms, wake): (i64, i64, i64, i32, i64),
) -> Result<i64, i32> {
    let conn = sh.stream(h)?;
    if cap <= 0 || !timeout_ok(timeout_ms) {
        return Err(status::ERR_INVALID);
    }
    let n = cap.min(i64::try_from(sh.max_bytes).unwrap_or(i64::MAX));
    // Refuse a bad output buffer before any byte is consumed from the socket.
    check_guest_range(caller, ptr, n)?;
    let wake = sh.wake(wake)?;
    let remaining = caller.data().deadline_remaining();
    let mut buf = vec![0u8; n as usize];
    let got = guarded(
        async {
            loop {
                conn.tcp.readable().await.map_err(|e| io_status(&e))?;
                match conn.tcp.try_read(&mut buf) {
                    Ok(0) => return Err(status::ERR_EOF),
                    Ok(k) => return Ok(k),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(io_status(&e)),
                }
            }
        },
        timeout_ms,
        remaining,
        wake.as_ref(),
    )
    .await?;
    write_guest_bytes(caller, ptr, &buf[..got])?;
    Ok(got as i64)
}

async fn send(
    sh: &NetShared,
    caller: &mut Caller<'_, HostState>,
    (h, ptr, len, timeout_ms, wake): (i64, i64, i64, i32, i64),
) -> Result<i64, i32> {
    let conn = sh.stream(h)?;
    if len < 0 || !timeout_ok(timeout_ms) {
        return Err(status::ERR_INVALID);
    }
    let n = len.min(i64::try_from(sh.max_bytes).unwrap_or(i64::MAX));
    let data = read_guest_bytes(caller, ptr, n, sh.max_bytes)?;
    let wake = sh.wake(wake)?;
    if data.is_empty() {
        return Ok(0);
    }
    let remaining = caller.data().deadline_remaining();
    let sent = guarded(
        async {
            loop {
                conn.tcp.writable().await.map_err(|e| io_status(&e))?;
                match conn.tcp.try_write(&data) {
                    Ok(k) => return Ok(k),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(io_status(&e)),
                }
            }
        },
        timeout_ms,
        remaining,
        wake.as_ref(),
    )
    .await?;
    Ok(sent as i64)
}

async fn dns_resolve(
    sh: &NetShared,
    caller: &mut Caller<'_, HostState>,
    (ptr, len, out, cap): (i64, i64, i64, i64),
) -> Result<i64, i32> {
    let name = text(caller, ptr, len)?;
    if cap <= 0 {
        return Err(status::ERR_INVALID);
    }
    check_guest_range(
        caller,
        out,
        cap.min(i64::try_from(sh.max_bytes).unwrap_or(i64::MAX)),
    )?;
    if !sh.connect.iter().any(|s| s.host_matches(&name)) {
        return Err(status::ERR_DENIED);
    }
    let found = tokio::time::timeout(DNS_TIMEOUT, tokio::net::lookup_host((name.as_str(), 0)))
        .await
        .map_err(|_| status::ERR_TIMEOUT)?
        .map_err(|_| status::ERR_NOT_FOUND)?;
    let mut seen = Vec::new();
    let mut outb = String::new();
    for a in found {
        let ip = a.ip();
        if !seen.contains(&ip) {
            seen.push(ip);
            let line = format!("{ip}\n");
            if (outb.len() + line.len()) as i64 > cap {
                break;
            }
            outb.push_str(&line);
        }
    }
    if outb.is_empty() {
        return Err(if seen.is_empty() {
            status::ERR_NOT_FOUND
        } else {
            status::ERR_TOO_LARGE
        });
    }
    write_guest_bytes(caller, out, outb.as_bytes())?;
    Ok(outb.len() as i64)
}

fn peer(
    sh: &NetShared,
    caller: &mut Caller<'_, HostState>,
    (h, ptr, cap): (i64, i64, i64),
) -> Result<i64, i32> {
    let conn = sh.stream(h)?;
    let t = conn.peer.ip().to_string();
    if cap < 0 {
        return Err(status::ERR_INVALID);
    }
    if t.len() as i64 > cap {
        return Err(status::ERR_TOO_LARGE);
    }
    write_guest_bytes(caller, ptr, t.as_bytes())?;
    Ok(t.len() as i64)
}

fn shutdown(sh: &NetShared, (h, how): (i64, i32)) -> Result<i32, i32> {
    let conn = sh.stream(h)?;
    let how = match how {
        0 => Shutdown::Read,
        1 => Shutdown::Write,
        2 => Shutdown::Both,
        _ => return Err(status::ERR_INVALID),
    };
    socket2::SockRef::from(&conn.tcp)
        .shutdown(how)
        .map_err(|e| io_status(&e))?;
    Ok(0)
}

impl Capability for NetCapability {
    fn id(&self) -> CapabilityId {
        CapabilityId::net_v1()
    }

    #[allow(clippy::too_many_lines)]
    fn link(&self, linker: &mut Linker<HostState>) -> wasmtime::Result<()> {
        let id = self.id();
        let m = id.import_module();

        // Registers an async function whose body is `$f(&shared, &mut caller, args)`.
        macro_rules! async_fn {
            ($name:literal, $args:ty, $f:ident) => {{
                let id = id.clone();
                linker.func_wrap_async(
                    &m,
                    $name,
                    move |mut caller: Caller<'_, HostState>, args: $args| {
                        let id = id.clone();
                        Box::new(async move {
                            let r: Result<_, i32> = match shared(&id, &mut caller) {
                                Some(sh) => $f(&sh, &mut caller, args).await,
                                None => Err(status::ERR_IO),
                            };
                            Ok(code(r))
                        })
                    },
                )?;
            }};
        }
        async_fn!("tcp_listen", (i64, i64, i32, i32, i32), tcp_listen);
        async_fn!("tcp_accept", (i64, i32, i64), tcp_accept);
        async_fn!("tcp_connect", (i64, i64, i32, i32, i64), tcp_connect);
        async_fn!("recv", (i64, i64, i64, i32, i64), recv);
        async_fn!("send", (i64, i64, i64, i32, i64), send);
        async_fn!("dns_resolve", (i64, i64, i64, i64), dns_resolve);

        // Synchronous functions.
        {
            let id = id.clone();
            linker.func_wrap(
                &m,
                "local_port",
                move |mut caller: Caller<'_, HostState>, h: i64| -> i32 {
                    let Some(sh) = shared(&id, &mut caller) else {
                        return status::ERR_IO;
                    };
                    code32(sh.sock(h).and_then(|s| {
                        let a = match s {
                            SockRef::Listener(l) => l.local_addr(),
                            SockRef::Stream(c) => c.tcp.local_addr(),
                        };
                        a.map(|a| i32::from(a.port())).map_err(|e| io_status(&e))
                    }))
                },
            )?;
        }
        {
            let id = id.clone();
            linker.func_wrap(
                &m,
                "peer",
                move |mut caller: Caller<'_, HostState>, h: i64, ptr: i64, cap: i64| -> i64 {
                    let Some(sh) = shared(&id, &mut caller) else {
                        return i64::from(status::ERR_IO);
                    };
                    code(peer(&sh, &mut caller, (h, ptr, cap)))
                },
            )?;
        }
        {
            let id = id.clone();
            linker.func_wrap(
                &m,
                "peer_port",
                move |mut caller: Caller<'_, HostState>, h: i64| -> i32 {
                    let Some(sh) = shared(&id, &mut caller) else {
                        return status::ERR_IO;
                    };
                    code32(sh.stream(h).map(|c| i32::from(c.peer.port())))
                },
            )?;
        }
        {
            let id = id.clone();
            linker.func_wrap(
                &m,
                "shutdown",
                move |mut caller: Caller<'_, HostState>, h: i64, how: i32| -> i32 {
                    let Some(sh) = shared(&id, &mut caller) else {
                        return status::ERR_IO;
                    };
                    code32(shutdown(&sh, (h, how)))
                },
            )?;
        }
        {
            let id = id.clone();
            linker.func_wrap(
                &m,
                "close",
                move |mut caller: Caller<'_, HostState>, h: i64| -> i32 {
                    let Some(sh) = shared(&id, &mut caller) else {
                        return status::ERR_IO;
                    };
                    // Dropping the last reference closes the socket; a wait that is still
                    // running in another call keeps it alive until it returns.
                    match lock(&sh.socks).remove(h) {
                        Some(_) => status::OK,
                        None => status::ERR_BAD_HANDLE,
                    }
                },
            )?;
        }
        {
            let id = id.clone();
            linker.func_wrap(
                &m,
                "wake_open",
                move |mut caller: Caller<'_, HostState>, parent: i64| -> i64 {
                    let Some(sh) = shared(&id, &mut caller) else {
                        return i64::from(status::ERR_IO);
                    };
                    code(sh.wake(parent).and_then(|p| {
                        let node = p.map_or_else(|| Arc::new(WakeNode::default()), |p| p.child());
                        lock(&sh.wakes).insert(node).map_err(|_| status::ERR_QUOTA)
                    }))
                },
            )?;
        }
        {
            let id = id.clone();
            linker.func_wrap(
                &m,
                "wake_signal",
                move |mut caller: Caller<'_, HostState>, h: i64| -> i32 {
                    let Some(sh) = shared(&id, &mut caller) else {
                        return status::ERR_IO;
                    };
                    code32(match sh.wake(h) {
                        Ok(Some(n)) => {
                            n.signal();
                            Ok(0)
                        }
                        Ok(None) => Err(status::ERR_BAD_HANDLE),
                        Err(e) => Err(e),
                    })
                },
            )?;
        }
        {
            let id = id.clone();
            linker.func_wrap(
                &m,
                "wake_check",
                move |mut caller: Caller<'_, HostState>, h: i64| -> i32 {
                    let Some(sh) = shared(&id, &mut caller) else {
                        return status::ERR_IO;
                    };
                    code32(match sh.wake(h) {
                        Ok(Some(n)) => Ok(i32::from(n.is_set())),
                        Ok(None) => Err(status::ERR_BAD_HANDLE),
                        Err(e) => Err(e),
                    })
                },
            )?;
        }
        {
            let id = id.clone();
            linker.func_wrap(
                &m,
                "wake_close",
                move |mut caller: Caller<'_, HostState>, h: i64| -> i32 {
                    let Some(sh) = shared(&id, &mut caller) else {
                        return status::ERR_IO;
                    };
                    match lock(&sh.wakes).remove(h) {
                        Some(_) => status::OK,
                        None => status::ERR_BAD_HANDLE,
                    }
                },
            )?;
        }
        Ok(())
    }

    fn instance_state(
        &self,
        info: &InstanceInfo,
        config: &serde_json::Value,
    ) -> Box<dyn Any + Send> {
        Box::new(NetState(Arc::new(NetShared::new(info, config))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_rules() {
        let s = NetScope::parse("*.Example.com:443").unwrap();
        assert!(s.covers("a.example.com", 443));
        assert!(s.covers("A.EXAMPLE.COM", 443));
        assert!(!s.covers("example.com", 443));
        assert!(!s.covers("a.example.com", 80));
        let s = NetScope::parse("127.0.0.1:0-65535").unwrap();
        assert!(s.covers("127.0.0.1", 0) && s.covers("127.0.0.1", 9));
        assert!(!s.covers("127.0.0.2", 9));
        assert!(NetScope::parse("[::1]:80").unwrap().covers("::1", 80));
        assert!(NetScope::parse("*:1-9").unwrap().covers("anything", 5));
        for bad in ["", "host", "a*b:80", "h:99999", "h:5-1", ":80", "*.a*:80"] {
            assert!(NetScope::parse(bad).is_err(), "{bad}");
        }
    }
}
