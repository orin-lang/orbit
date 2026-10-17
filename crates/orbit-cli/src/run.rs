use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args;
use orbit_abi::CapabilityId;
use orbit_caps::{
    ClockCapability, InMemoryKv, KvCapability, LogCapability, RandomCapability, StderrSink,
};
use orbit_runtime::{CapabilityRegistry, Grants, Limits, Runtime, RuntimeConfig};

#[derive(Args)]
pub struct RunArgs {
    /// Path to a .wasm or .wat file.
    module: PathBuf,
    /// Exported function to call after initialisation.
    #[arg(long, value_name = "NAME")]
    invoke: Option<String>,
    /// Integer argument for --invoke (repeat for several).
    #[arg(
        long = "arg",
        value_name = "N",
        allow_hyphen_values = true,
        requires = "invoke"
    )]
    args: Vec<i64>,
    /// Grant a capability, e.g. `log/v1` (repeatable). Nothing is granted by default.
    #[arg(long = "grant", value_name = "CAP")]
    grants: Vec<String>,
    /// KV namespace the module may open with `kv/v1` (repeatable).
    #[arg(long = "kv-namespace", value_name = "NAME")]
    kv_namespaces: Vec<String>,
    /// Maximum linear memory in bytes.
    #[arg(long, value_name = "BYTES")]
    max_memory: Option<u64>,
    /// Fuel per invocation (instruction budget).
    #[arg(long, value_name = "N")]
    fuel: Option<u64>,
    /// Wall-clock timeout per invocation in milliseconds.
    #[arg(long, value_name = "MS")]
    timeout_ms: Option<u64>,
    /// Tenant id attached to the isolate's log records.
    #[arg(long, default_value = "cli", value_name = "ID")]
    tenant: String,
}

/// The capabilities this binary can serve.
pub fn registry() -> CapabilityRegistry {
    CapabilityRegistry::new()
        .with(LogCapability::new(StderrSink))
        .with(ClockCapability::system())
        .with(RandomCapability::new())
        .with(KvCapability::new(InMemoryKv::new()))
}

pub async fn run(args: RunArgs) -> Result<()> {
    let mut grants = Grants::none();
    for g in &args.grants {
        let id: CapabilityId = g.parse().with_context(|| format!("bad --grant `{g}`"))?;
        if orbit_abi::spec(&id).is_none() {
            bail!(
                "bad --grant `{g}`: no such capability in ABI v{}",
                orbit_abi::ABI_VERSION
            );
        }
        grants = if id == CapabilityId::kv_v1() {
            grants.grant_with(id, serde_json::json!({ "namespaces": args.kv_namespaces }))
        } else {
            grants.grant(id)
        };
    }
    if !args.kv_namespaces.is_empty() && !grants.contains(&CapabilityId::kv_v1()) {
        bail!("--kv-namespace requires --grant kv/v1");
    }
    let mut limits = Limits::default();
    if let Some(m) = args.max_memory {
        limits.max_memory_bytes = m;
    }
    limits.fuel = args.fuel;
    limits.timeout = args.timeout_ms.map(Duration::from_millis);

    let bytes = std::fs::read(&args.module)
        .with_context(|| format!("cannot read {}", args.module.display()))?;
    let runtime = Runtime::new(RuntimeConfig::default(), registry())?;
    let artifact = runtime.load(&bytes).context("load failed")?;
    let mut isolate = runtime
        .spawn_isolate(&artifact, args.tenant.as_str(), &grants, limits)
        .await
        .context("instantiate failed")?;
    isolate.init().await.context("initialisation failed")?;

    if let Some(name) = &args.invoke {
        let results = isolate
            .invoke(name, &args.args)
            .await
            .with_context(|| format!("`{name}` failed"))?;
        for r in results {
            println!("{r}");
        }
    }
    isolate.terminate();
    Ok(())
}
