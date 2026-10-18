//! The capability extension point: [`Capability`], [`CapabilityRegistry`] and [`Grants`].

use std::any::Any;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use orbit_abi::CapabilityId;
use wasmtime::Linker;

use crate::HostState;

/// Identity of one isolate, unique within a [`Runtime`](crate::Runtime).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InstanceId(pub u64);

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "i-{}", self.0)
    }
}

/// Identity of the tenant that owns an isolate. Attached by the host, never by the guest.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TenantId(Arc<str>);

impl TenantId {
    /// Creates a tenant id.
    #[must_use]
    pub fn new(id: impl AsRef<str>) -> Self {
        Self(Arc::from(id.as_ref()))
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TenantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for TenantId {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

impl From<&str> for TenantId {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

/// Facts about an isolate that a capability may use when creating its per-instance state.
#[derive(Clone, Debug)]
pub struct InstanceInfo {
    /// The isolate's id.
    pub instance_id: InstanceId,
    /// The owning tenant.
    pub tenant: TenantId,
    /// Per-invocation log byte budget from the isolate's [`Limits`](crate::Limits).
    pub max_log_bytes: u64,
    /// All capabilities granted to this isolate, with their configuration. A capability that
    /// depends on another grant (the WASI shim's clock and random functions) reads it here.
    pub grants: Grants,
}

/// A host capability: a set of functions in one `orbit:<name>/v<major>` import module.
///
/// Implementations live in `orbit-caps`. The same `Capability` object is shared by every
/// isolate, so per-instance data belongs in the value returned by
/// [`instance_state`](Capability::instance_state), which the host stores in the isolate's
/// [`HostState`] and which host functions reach with [`HostState::cap_state_mut`].
pub trait Capability: Send + Sync + 'static {
    /// The capability's id; also its import module name via [`CapabilityId::import_module`].
    fn id(&self) -> CapabilityId;

    /// Registers the capability's functions in `linker` under its import module name.
    ///
    /// # Errors
    /// Fails if a function cannot be defined.
    fn link(&self, linker: &mut Linker<HostState>) -> wasmtime::Result<()>;

    /// Creates the state of this capability for one isolate. `config` is the opaque
    /// per-capability configuration of the grant ([`serde_json::Value::Null`] if none).
    fn instance_state(
        &self,
        info: &InstanceInfo,
        config: &serde_json::Value,
    ) -> Box<dyn Any + Send>;

    /// Creates the state of this capability for one more guest thread of an isolate
    /// (`orbit:thread/v1`), given the state `parent` of the thread that spawns it.
    ///
    /// **Capabilities whose state is a resource of the isolate (handles, quotas, budgets)
    /// must override this and return a value that shares that state with `parent`** (an `Arc`
    /// inside the state type, see `orbit:net/v1`). The default creates a fresh, independent
    /// state with [`instance_state`](Capability::instance_state): every thread then has its
    /// own handle table and budgets, which is only right for stateless capabilities.
    fn thread_state(
        &self,
        _parent: &mut (dyn Any + Send),
        info: &InstanceInfo,
        config: &serde_json::Value,
    ) -> Box<dyn Any + Send> {
        self.instance_state(info, config)
    }

    /// Called before every invocation (including initialisation); reset per-invocation
    /// budgets here.
    fn begin_invocation(&self, _state: &mut (dyn Any + Send)) {}

    /// Called after every invocation (including initialisation), whether it returned,
    /// trapped or exited; flush buffered output here. Not called when the isolate could not
    /// start running.
    fn end_invocation(&self, _state: &mut (dyn Any + Send)) {}
}

/// The capabilities a host implements.
#[derive(Clone, Default)]
pub struct CapabilityRegistry {
    caps: HashMap<CapabilityId, Arc<dyn Capability>>,
}

impl CapabilityRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers (or replaces) a capability.
    pub fn register(&mut self, cap: impl Capability) -> &mut Self {
        let cap: Arc<dyn Capability> = Arc::new(cap);
        self.caps.insert(cap.id(), cap);
        self
    }

    /// Builder-style [`register`](Self::register).
    #[must_use]
    pub fn with(mut self, cap: impl Capability) -> Self {
        self.register(cap);
        self
    }

    /// Looks a capability up.
    #[must_use]
    pub fn get(&self, id: &CapabilityId) -> Option<&Arc<dyn Capability>> {
        self.caps.get(id)
    }

    /// Whether the capability is registered.
    #[must_use]
    pub fn contains(&self, id: &CapabilityId) -> bool {
        self.caps.contains_key(id)
    }

    /// Ids of all registered capabilities, sorted.
    #[must_use]
    pub fn ids(&self) -> Vec<CapabilityId> {
        let mut ids: Vec<_> = self.caps.keys().cloned().collect();
        ids.sort();
        ids
    }
}

impl fmt::Debug for CapabilityRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set()
            .entries(self.ids().iter().map(ToString::to_string))
            .finish()
    }
}

/// The capabilities one isolate is allowed to use, each with an opaque configuration.
///
/// Deny by default: [`Grants::default`] grants nothing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Grants {
    granted: BTreeMap<CapabilityId, serde_json::Value>,
}

impl Grants {
    /// No grants.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Grants a capability with no configuration.
    #[must_use]
    pub fn grant(self, id: CapabilityId) -> Self {
        self.grant_with(id, serde_json::Value::Null)
    }

    /// Grants a capability with an opaque configuration (for example KV namespaces).
    #[must_use]
    pub fn grant_with(mut self, id: CapabilityId, config: serde_json::Value) -> Self {
        self.granted.insert(id, config);
        self
    }

    /// Whether `id` is granted.
    #[must_use]
    pub fn contains(&self, id: &CapabilityId) -> bool {
        self.granted.contains_key(id)
    }

    /// The configuration of a granted capability.
    #[must_use]
    pub fn config(&self, id: &CapabilityId) -> Option<&serde_json::Value> {
        self.granted.get(id)
    }

    /// Granted ids in order.
    pub fn ids(&self) -> impl Iterator<Item = &CapabilityId> {
        self.granted.keys()
    }

    /// Number of granted capabilities.
    #[must_use]
    pub fn len(&self) -> usize {
        self.granted.len()
    }

    /// Whether nothing is granted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.granted.is_empty()
    }
}

impl FromIterator<CapabilityId> for Grants {
    fn from_iter<I: IntoIterator<Item = CapabilityId>>(iter: I) -> Self {
        iter.into_iter().fold(Grants::none(), Grants::grant)
    }
}
