use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args;
use orbit_abi::CapabilityId;
use orbit_caps::{
    ClockCapability, InMemoryKv, KvCapability, LogCapability, RandomCapability, StderrSink,
    WasiCapability,
};
use orbit_runtime::{CapabilityRegistry, ExecError, Grants, Limits, Runtime, RuntimeConfig};

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
    /// Argument for a module granted `wasi_p1/v1`, after argv[0] (`main`); repeatable.
    #[arg(long = "wasi-arg", value_name = "ARG", allow_hyphen_values = true)]
    wasi_args: Vec<String>,
    /// Environment variable `K=V` for a module granted `wasi_p1/v1`; repeatable. The host's
    /// own environment is never passed.
    #[arg(long = "wasi-env", value_name = "K=V")]
    wasi_env: Vec<String>,
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
    let clock = ClockCapability::system();
    // WASI clocks read the same source as `orbit:clock/v1`; guest stdout/stderr go to stderr.
    let wasi = WasiCapability::new(StderrSink).with_clock(&clock);
    CapabilityRegistry::new()
        .with(LogCapability::new(StderrSink))
        .with(clock)
        .with(RandomCapability::new())
        .with(KvCapability::new(InMemoryKv::new()))
        .with(wasi)
}

fn wasi_config(args: &RunArgs) -> Result<serde_json::Value> {
    let mut argv = vec!["main".to_owned()];
    argv.extend(args.wasi_args.iter().cloned());
    let mut env = serde_json::Map::new();
    for kv in &args.wasi_env {
        let (k, v) = kv
            .split_once('=')
            .filter(|(k, _)| !k.is_empty())
            .with_context(|| format!("bad --wasi-env `{kv}`: expected K=V"))?;
        env.insert(k.to_owned(), serde_json::Value::String(v.to_owned()));
    }
    Ok(serde_json::json!({ "args": argv, "env": env }))
}

/// Runs the module; returns the process exit code (the guest's `proc_exit` code, else 0).
pub async fn run(args: RunArgs) -> Result<i32> {
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
        } else if id == CapabilityId::wasi_p1_v1() {
            grants.grant_with(id, wasi_config(&args)?)
        } else {
            grants.grant(id)
        };
    }
    let wasi_flags = !args.wasi_args.is_empty() || !args.wasi_env.is_empty();
    if wasi_flags && !grants.contains(&CapabilityId::wasi_p1_v1()) {
        bail!("--wasi-arg/--wasi-env require --grant wasi_p1/v1");
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
    let has_start = artifact
        .info()
        .exports
        .iter()
        .any(|e| e.name == "_start" && e.kind == "func");
    let code = drive(&mut isolate, &args, has_start).await?;
    isolate.terminate();
    Ok(code)
}

/// Initialises the isolate and runs `--invoke NAME`, or `_start` of a command module.
async fn drive(
    isolate: &mut orbit_runtime::Isolate,
    args: &RunArgs,
    has_start: bool,
) -> Result<i32> {
    match isolate.init().await {
        Ok(()) => {}
        Err(ExecError::Exit(code)) => return Ok(code),
        Err(e) => return Err(e).context("initialisation failed"),
    }
    if let Some(code) = isolate.exit_code() {
        return Ok(code);
    }
    let entry = args.invoke.as_deref().unwrap_or("_start");
    if args.invoke.is_none() && !has_start {
        return Ok(0);
    }
    match isolate.invoke(entry, &args.args).await {
        Ok(results) => {
            for r in results {
                println!("{r}");
            }
            Ok(isolate.exit_code().unwrap_or(0))
        }
        Err(ExecError::Exit(code)) => Ok(code),
        Err(e) => Err(e).with_context(|| format!("`{entry}` failed")),
    }
}
