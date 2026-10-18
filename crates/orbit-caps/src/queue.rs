//! `orbit:queue/v1`: publish messages to named queues behind a host [`QueueBackend`].
//!
//! Grant config: `{"publish": ["orders", ...]}`. **Deny by default**: a grant without a
//! `publish` list (or an empty one) refuses every queue with `ERR_DENIED`. The tenant is taken
//! from the host ([`HostState::tenant`]) and is part of every backend call, so two tenants
//! publishing to `"orders"` never share a queue.
//!
//! The contract is deliberately publish-only and handle-free: a publish is a single
//! fire-and-forget call that returns once the backend has durably accepted the message
//! (at-least-once delivery to consumers is the backend's concern). Receiving is the host's
//! business (the embedder delivers messages to its own consumers), not a guest call, so the
//! capability keeps no per-instance state besides the grant.
//!
//! Semantics fixed by this implementation: queue names are 1 to 255 bytes of UTF-8; a message
//! is at most 1 MiB (may be empty); a batch has 1 to 256 messages, framed as repeated
//! `u32 little-endian length` + bytes, and is all-or-nothing as far as the backend allows.

use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use orbit_abi::{
    CapabilityId,
    limits::{QUEUE_MAX_BATCH_MESSAGES, QUEUE_MAX_MESSAGE_BYTES, QUEUE_MAX_NAME_BYTES},
    status,
};
use orbit_runtime::{Capability, HostState, InstanceInfo, read_guest_bytes};
use wasmtime::{Caller, Linker};

/// Upper bound on the framed size of one `publish_batch` buffer.
pub const QUEUE_MAX_BATCH_BYTES: u64 = 8 * 1024 * 1024;

/// Why a [`QueueBackend`] refused a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueueError {
    /// The queue is at capacity or the tenant is over its quota (reported to the guest as
    /// `ERR_QUOTA`; retrying later may succeed).
    Full,
    /// The backend failed (reported as `ERR_IO`).
    Backend(String),
}

impl fmt::Display for QueueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => f.write_str("queue is full"),
            Self::Backend(m) => write!(f, "queue backend error: {m}"),
        }
    }
}

impl std::error::Error for QueueError {}

/// A message queue shared by all isolates of a runtime. Every call carries the tenant and the
/// queue name; implementations must keep both part of the queue's identity. Implemented by the
/// embedder (SQLite, NATS JetStream, ...); [`InMemoryQueue`] is for tests.
pub trait QueueBackend: Send + Sync + 'static {
    /// Appends one message to `queue`.
    ///
    /// # Errors
    /// [`QueueError`] if the message cannot be accepted.
    fn publish(&self, tenant: &str, queue: &str, body: &[u8]) -> Result<(), QueueError>;

    /// Appends several messages in order. The default publishes one by one; backends with
    /// transactions should override it to make the batch atomic.
    ///
    /// # Errors
    /// [`QueueError`] if a message cannot be accepted.
    fn publish_batch(
        &self,
        tenant: &str,
        queue: &str,
        bodies: &[Vec<u8>],
    ) -> Result<(), QueueError> {
        for b in bodies {
            self.publish(tenant, queue, b)?;
        }
        Ok(())
    }
}

type QueueMap = HashMap<(String, String), Vec<Vec<u8>>>;

/// An in-process [`QueueBackend`] that records messages; clones share the same data.
#[derive(Clone, Debug, Default)]
pub struct InMemoryQueue {
    queues: Arc<Mutex<QueueMap>>,
    capacity: Option<usize>,
}

impl InMemoryQueue {
    /// An empty, unbounded backend.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A backend holding at most `capacity` messages per queue (further publishes are
    /// [`QueueError::Full`]).
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: Some(capacity),
            ..Self::default()
        }
    }

    /// The messages published so far to `tenant`'s `queue`, oldest first.
    #[must_use]
    pub fn messages(&self, tenant: &str, queue: &str) -> Vec<Vec<u8>> {
        self.queues
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(tenant.to_owned(), queue.to_owned()))
            .cloned()
            .unwrap_or_default()
    }
}

impl QueueBackend for InMemoryQueue {
    fn publish(&self, tenant: &str, queue: &str, body: &[u8]) -> Result<(), QueueError> {
        self.publish_batch(tenant, queue, &[body.to_vec()])
    }

    fn publish_batch(
        &self,
        tenant: &str,
        queue: &str,
        bodies: &[Vec<u8>],
    ) -> Result<(), QueueError> {
        let mut g = self.queues.lock().unwrap_or_else(PoisonError::into_inner);
        let q = g.entry((tenant.to_owned(), queue.to_owned())).or_default();
        if self.capacity.is_some_and(|c| q.len() + bodies.len() > c) {
            return Err(QueueError::Full);
        }
        q.extend(bodies.iter().cloned());
        Ok(())
    }
}

struct QueueState {
    publish: Vec<String>,
}

/// The `orbit:queue/v1` capability.
#[derive(Clone)]
pub struct QueueCapability {
    backend: Arc<dyn QueueBackend>,
}

impl QueueCapability {
    /// A queue capability over `backend`.
    #[must_use]
    pub fn new(backend: impl QueueBackend) -> Self {
        Self {
            backend: Arc::new(backend),
        }
    }

    /// Like [`new`](Self::new) for an already shared backend.
    #[must_use]
    pub fn with_shared_backend(backend: Arc<dyn QueueBackend>) -> Self {
        Self { backend }
    }
}

impl fmt::Debug for QueueCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueueCapability").finish_non_exhaustive()
    }
}

/// Reads and checks the queue name against the grant.
fn granted_queue(
    id: &CapabilityId,
    caller: &mut Caller<'_, HostState>,
    ptr: i64,
    len: i64,
) -> Result<String, i32> {
    let bytes = read_guest_bytes(caller, ptr, len, QUEUE_MAX_NAME_BYTES)?;
    let name = String::from_utf8(bytes).map_err(|_| status::ERR_INVALID)?;
    if name.is_empty() {
        return Err(status::ERR_INVALID);
    }
    let state = caller
        .data_mut()
        .cap_state_mut::<QueueState>(id)
        .ok_or(status::ERR_IO)?;
    if state.publish.contains(&name) {
        Ok(name)
    } else {
        Err(status::ERR_DENIED)
    }
}

fn backend_status(r: Result<(), QueueError>) -> i32 {
    match r {
        Ok(()) => status::OK,
        Err(QueueError::Full) => status::ERR_QUOTA,
        Err(QueueError::Backend(_)) => status::ERR_IO,
    }
}

fn publish(
    backend: &dyn QueueBackend,
    id: &CapabilityId,
    caller: &mut Caller<'_, HostState>,
    args: [i64; 4],
) -> i32 {
    let [qp, ql, bp, bl] = args;
    caller.data_mut().record_call(id);
    let queue = match granted_queue(id, caller, qp, ql) {
        Ok(q) => q,
        Err(code) => return code,
    };
    let body = match read_guest_bytes(caller, bp, bl, QUEUE_MAX_MESSAGE_BYTES) {
        Ok(b) => b,
        Err(code) => return code,
    };
    let tenant = caller.data().tenant().clone();
    backend_status(backend.publish(tenant.as_str(), &queue, &body))
}

/// Splits a `publish_batch` buffer into messages.
fn parse_batch(buf: &[u8]) -> Result<Vec<Vec<u8>>, i32> {
    let mut out = Vec::new();
    let mut rest = buf;
    while !rest.is_empty() {
        let (head, tail) = rest.split_first_chunk::<4>().ok_or(status::ERR_INVALID)?;
        let n = u32::from_le_bytes(*head) as usize;
        if n as u64 > QUEUE_MAX_MESSAGE_BYTES {
            return Err(status::ERR_TOO_LARGE);
        }
        if tail.len() < n {
            return Err(status::ERR_INVALID);
        }
        let (body, tail) = tail.split_at(n);
        out.push(body.to_vec());
        if out.len() > QUEUE_MAX_BATCH_MESSAGES {
            return Err(status::ERR_TOO_LARGE);
        }
        rest = tail;
    }
    if out.is_empty() {
        return Err(status::ERR_INVALID);
    }
    Ok(out)
}

fn publish_batch(
    backend: &dyn QueueBackend,
    id: &CapabilityId,
    caller: &mut Caller<'_, HostState>,
    args: [i64; 4],
) -> i32 {
    let [qp, ql, bp, bl] = args;
    caller.data_mut().record_call(id);
    let queue = match granted_queue(id, caller, qp, ql) {
        Ok(q) => q,
        Err(code) => return code,
    };
    let buf = match read_guest_bytes(caller, bp, bl, QUEUE_MAX_BATCH_BYTES) {
        Ok(b) => b,
        Err(code) => return code,
    };
    let bodies = match parse_batch(&buf) {
        Ok(b) => b,
        Err(code) => return code,
    };
    let tenant = caller.data().tenant().clone();
    backend_status(backend.publish_batch(tenant.as_str(), &queue, &bodies))
}

impl Capability for QueueCapability {
    fn id(&self) -> CapabilityId {
        CapabilityId::queue_v1()
    }

    fn link(&self, linker: &mut Linker<HostState>) -> wasmtime::Result<()> {
        let id = self.id();
        let m = id.import_module();
        let (i, b) = (id.clone(), Arc::clone(&self.backend));
        linker.func_wrap(
            &m,
            "publish",
            move |mut c: Caller<'_, HostState>, qp: i64, ql: i64, bp: i64, bl: i64| -> i32 {
                publish(&*b, &i, &mut c, [qp, ql, bp, bl])
            },
        )?;
        let b = Arc::clone(&self.backend);
        linker.func_wrap(
            &m,
            "publish_batch",
            move |mut c: Caller<'_, HostState>, qp: i64, ql: i64, bp: i64, bl: i64| -> i32 {
                publish_batch(&*b, &id, &mut c, [qp, ql, bp, bl])
            },
        )?;
        Ok(())
    }

    fn instance_state(
        &self,
        _info: &InstanceInfo,
        config: &serde_json::Value,
    ) -> Box<dyn Any + Send> {
        let publish = config
            .get("publish")
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        Box::new(QueueState { publish })
    }
}
