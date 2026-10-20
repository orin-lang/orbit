//! `orbit:kv/v1`: key-value storage behind a shared [`KvStore`] adapter.
//!
//! Grant config: `{"namespaces": ["cache", ...]}`. The tenant is taken from the host
//! ([`HostState::tenant`]) and is part of every adapter call, so tenants never share keys.
//! Handles live in a per-instance [`HandleTable`] and are dropped with the instance.
//!
//! Semantics fixed by this implementation: `delete` of an absent key returns `OK`
//! (idempotent); an empty key is `ERR_INVALID`.

use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use orbit_abi::{
    CapabilityId,
    limits::{KV_MAX_KEY_BYTES, KV_MAX_VALUE_BYTES},
    status,
};
use orbit_runtime::{
    Capability, HandleTable, HostState, InstanceInfo, read_guest_bytes, write_guest_bytes,
};
use wasmtime::{Caller, Linker};

/// Maximum number of open KV handles per instance.
pub const KV_MAX_HANDLES: usize = 64;

/// The storage adapter failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KvError(pub String);

impl fmt::Display for KvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "kv adapter error: {}", self.0)
    }
}

impl std::error::Error for KvError {}

/// A key-value store shared by all isolates of a runtime. Every call carries the tenant and
/// the namespace; implementations must keep them part of the storage key.
pub trait KvStore: Send + Sync + 'static {
    /// The value of `key`, if present.
    ///
    /// # Errors
    /// [`KvError`] if the backend fails.
    fn get(&self, tenant: &str, namespace: &str, key: &[u8]) -> Result<Option<Vec<u8>>, KvError>;

    /// Stores `value` under `key`.
    ///
    /// # Errors
    /// [`KvError`] if the backend fails.
    fn put(&self, tenant: &str, namespace: &str, key: &[u8], value: &[u8]) -> Result<(), KvError>;

    /// Removes `key` (absent keys are not an error).
    ///
    /// # Errors
    /// [`KvError`] if the backend fails.
    fn delete(&self, tenant: &str, namespace: &str, key: &[u8]) -> Result<(), KvError>;
}

type MemKey = (String, String, Vec<u8>);

/// An in-process [`KvStore`]; clones share the same data.
#[derive(Clone, Debug, Default)]
pub struct InMemoryKv {
    data: Arc<RwLock<HashMap<MemKey, Vec<u8>>>>,
}

impl InMemoryKv {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn key(tenant: &str, namespace: &str, key: &[u8]) -> MemKey {
        (tenant.to_owned(), namespace.to_owned(), key.to_vec())
    }
}

impl KvStore for InMemoryKv {
    fn get(&self, tenant: &str, namespace: &str, key: &[u8]) -> Result<Option<Vec<u8>>, KvError> {
        let g = self
            .data
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(g.get(&Self::key(tenant, namespace, key)).cloned())
    }

    fn put(&self, tenant: &str, namespace: &str, key: &[u8], value: &[u8]) -> Result<(), KvError> {
        self.data
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(Self::key(tenant, namespace, key), value.to_vec());
        Ok(())
    }

    fn delete(&self, tenant: &str, namespace: &str, key: &[u8]) -> Result<(), KvError> {
        self.data
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&Self::key(tenant, namespace, key));
        Ok(())
    }
}

/// Threads of an isolate share the namespace list and the handle table.
struct KvState(Arc<KvShared>);

struct KvShared {
    allowed: Vec<String>,
    handles: Mutex<HandleTable<Arc<str>>>,
}

fn handles(s: &KvShared) -> MutexGuard<'_, HandleTable<Arc<str>>> {
    s.handles
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The `orbit:kv/v1` capability.
#[derive(Clone)]
pub struct KvCapability {
    store: Arc<dyn KvStore>,
}

impl KvCapability {
    /// A KV capability over `store`.
    #[must_use]
    pub fn new(store: impl KvStore) -> Self {
        Self {
            store: Arc::new(store),
        }
    }

    /// Like [`new`](Self::new) for an already shared store.
    #[must_use]
    pub fn with_shared_store(store: Arc<dyn KvStore>) -> Self {
        Self { store }
    }
}

impl fmt::Debug for KvCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KvCapability").finish_non_exhaustive()
    }
}

/// Resolves a handle to its namespace.
fn namespace_of(host: &mut HostState, id: &CapabilityId, h: i64) -> Result<Arc<str>, i32> {
    let s = host.cap_state_mut::<KvState>(id).ok_or(status::ERR_IO)?;
    handles(&s.0).get(h).cloned().ok_or(status::ERR_BAD_HANDLE)
}

fn open(id: &CapabilityId, caller: &mut Caller<'_, HostState>, ptr: i64, len: i64) -> i64 {
    caller.data_mut().record_call(id);
    let bytes = match read_guest_bytes(caller, ptr, len, KV_MAX_KEY_BYTES) {
        Ok(b) => b,
        Err(code) => return i64::from(code),
    };
    let Ok(name) = String::from_utf8(bytes) else {
        return i64::from(status::ERR_INVALID);
    };
    let Some(state) = caller.data_mut().cap_state_mut::<KvState>(id) else {
        return i64::from(status::ERR_IO);
    };
    if !state.0.allowed.contains(&name) {
        return i64::from(status::ERR_DENIED);
    }
    match handles(&state.0).insert(Arc::from(name)) {
        Ok(h) => h,
        Err(_) => i64::from(status::ERR_QUOTA),
    }
}

fn get(
    store: &dyn KvStore,
    id: &CapabilityId,
    caller: &mut Caller<'_, HostState>,
    args: [i64; 5],
) -> i64 {
    let [h, key_ptr, key_len, out_ptr, out_cap] = args;
    caller.data_mut().record_call(id);
    let ns = match namespace_of(caller.data_mut(), id, h) {
        Ok(ns) => ns,
        Err(code) => return i64::from(code),
    };
    let key = match read_key(caller, key_ptr, key_len) {
        Ok(k) => k,
        Err(code) => return i64::from(code),
    };
    if out_cap < 0 {
        return i64::from(status::ERR_INVALID);
    }
    let tenant = caller.data().tenant().clone();
    let value = match store.get(tenant.as_str(), &ns, &key) {
        Ok(Some(v)) => v,
        Ok(None) => return i64::from(status::ERR_NOT_FOUND),
        Err(_) => return i64::from(status::ERR_IO),
    };
    let len = value.len() as i64;
    if out_cap >= len
        && let Err(code) = write_guest_bytes(caller, out_ptr, &value)
    {
        return i64::from(code);
    }
    len
}

fn read_key(caller: &mut Caller<'_, HostState>, ptr: i64, len: i64) -> Result<Vec<u8>, i32> {
    let key = read_guest_bytes(caller, ptr, len, KV_MAX_KEY_BYTES)?;
    if key.is_empty() {
        return Err(status::ERR_INVALID);
    }
    Ok(key)
}

fn put(
    store: &dyn KvStore,
    id: &CapabilityId,
    caller: &mut Caller<'_, HostState>,
    args: [i64; 5],
) -> i32 {
    let [h, key_ptr, key_len, val_ptr, val_len] = args;
    caller.data_mut().record_call(id);
    let ns = match namespace_of(caller.data_mut(), id, h) {
        Ok(ns) => ns,
        Err(code) => return code,
    };
    let key = match read_key(caller, key_ptr, key_len) {
        Ok(k) => k,
        Err(code) => return code,
    };
    let value = match read_guest_bytes(caller, val_ptr, val_len, KV_MAX_VALUE_BYTES) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let tenant = caller.data().tenant().clone();
    match store.put(tenant.as_str(), &ns, &key, &value) {
        Ok(()) => status::OK,
        Err(_) => status::ERR_IO,
    }
}

fn delete(
    store: &dyn KvStore,
    id: &CapabilityId,
    caller: &mut Caller<'_, HostState>,
    h: i64,
    key_ptr: i64,
    key_len: i64,
) -> i32 {
    caller.data_mut().record_call(id);
    let ns = match namespace_of(caller.data_mut(), id, h) {
        Ok(ns) => ns,
        Err(code) => return code,
    };
    let key = match read_key(caller, key_ptr, key_len) {
        Ok(k) => k,
        Err(code) => return code,
    };
    let tenant = caller.data().tenant().clone();
    match store.delete(tenant.as_str(), &ns, &key) {
        Ok(()) => status::OK,
        Err(_) => status::ERR_IO,
    }
}

fn close(id: &CapabilityId, caller: &mut Caller<'_, HostState>, h: i64) -> i32 {
    caller.data_mut().record_call(id);
    let Some(state) = caller.data_mut().cap_state_mut::<KvState>(id) else {
        return status::ERR_IO;
    };
    if handles(&state.0).remove(h).is_some() {
        status::OK
    } else {
        status::ERR_BAD_HANDLE
    }
}

impl Capability for KvCapability {
    fn id(&self) -> CapabilityId {
        CapabilityId::kv_v1()
    }

    fn link(&self, linker: &mut Linker<HostState>) -> wasmtime::Result<()> {
        let id = self.id();
        let m = id.import_module();
        let i = id.clone();
        linker.func_wrap(
            &m,
            "open",
            move |mut c: Caller<'_, HostState>, p: i64, l: i64| -> i64 { open(&i, &mut c, p, l) },
        )?;
        let (i, s) = (id.clone(), Arc::clone(&self.store));
        linker.func_wrap(
            &m,
            "get",
            move |mut c: Caller<'_, HostState>,
                  h: i64,
                  kp: i64,
                  kl: i64,
                  op: i64,
                  oc: i64|
                  -> i64 { get(&*s, &i, &mut c, [h, kp, kl, op, oc]) },
        )?;
        let (i, s) = (id.clone(), Arc::clone(&self.store));
        linker.func_wrap(
            &m,
            "put",
            move |mut c: Caller<'_, HostState>,
                  h: i64,
                  kp: i64,
                  kl: i64,
                  vp: i64,
                  vl: i64|
                  -> i32 { put(&*s, &i, &mut c, [h, kp, kl, vp, vl]) },
        )?;
        let (i, s) = (id.clone(), Arc::clone(&self.store));
        linker.func_wrap(
            &m,
            "delete",
            move |mut c: Caller<'_, HostState>, h: i64, kp: i64, kl: i64| -> i32 {
                delete(&*s, &i, &mut c, h, kp, kl)
            },
        )?;
        linker.func_wrap(
            &m,
            "close",
            move |mut c: Caller<'_, HostState>, h: i64| -> i32 { close(&id, &mut c, h) },
        )?;
        Ok(())
    }

    fn instance_state(
        &self,
        info: &InstanceInfo,
        config: &serde_json::Value,
    ) -> Box<dyn Any + Send> {
        let allowed = config
            .get("namespaces")
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        Box::new(KvState(Arc::new(KvShared {
            allowed,
            handles: Mutex::new(HandleTable::new(info.instance_id.0, KV_MAX_HANDLES)),
        })))
    }

    fn thread_state(
        &self,
        parent: &mut (dyn Any + Send),
        info: &InstanceInfo,
        config: &serde_json::Value,
    ) -> Box<dyn Any + Send> {
        match parent.downcast_ref::<KvState>() {
            Some(s) => Box::new(KvState(Arc::clone(&s.0))),
            None => self.instance_state(info, config),
        }
    }
}
