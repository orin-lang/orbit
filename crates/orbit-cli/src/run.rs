use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args;
use orbit_abi::CapabilityId;
use orbit_caps::{
    ClockCapability, InMemoryKv, KvCapability, LogCapability, NetCapability, RandomCapability,
    StderrSink, WasiCapability, net_grant_config,
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
    /// Network authority is granted with `net:listen=HOST:PORT[-PORT]` and
    /// `net:connect=HOST:PORT[-PORT]` (HOST is `*`, `*.suffix` or exact; repeatable; each one
    /// implies `net/v1`).
    #[arg(long = "grant", value_name = "CAP")]
    grants: Vec<String>,
    /// Allow listening on `ADDR:PORT` or `ADDR:LO-HI` (implies `--grant net/v1`); repeatable.
    /// Same as `--grant net:listen=ADDR:PORT`.
    #[arg(long = "listen", value_name = "ADDR:PORT")]
    listen: Vec<String>,
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
    /// Fuel per invocation (instruction budget). Passing it compiles fuel metering into the
    /// module for this run (about 1.2-1.8x slower on compute-heavy code); without it the run
    /// is unmetered.
    #[arg(long, value_name = "N")]
    fuel: Option<u64>,
    /// Wall-clock timeout per invocation in milliseconds; 0 = none. Without --fuel and
    /// without --timeout-ms there is no CPU bound at all: a runaway guest runs until it is
    /// killed. Network waits (`orbit:net/v1`) count against it: a server that blocks in
    /// accept needs `0` or no flag.
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
        .with(NetCapability::new())
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
    let (mut net_listen, mut net_connect) = (args.listen.clone(), Vec::new());
    for g in &args.grants {
        if let Some((kind, scope)) = g
            .strip_prefix("net:")
            .and_then(|r| r.split_once('='))
            .filter(|(k, _)| matches!(*k, "listen" | "connect"))
        {
            orbit_caps::NetScope::parse(scope).map_err(|e| anyhow::anyhow!("bad --grant: {e}"))?;
            if kind == "listen" {
                net_listen.push(scope.to_owned());
            } else {
                net_connect.push(scope.to_owned());
            }
            continue;
        }
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
    for scope in &args.listen {
        orbit_caps::NetScope::parse(scope).map_err(|e| anyhow::anyhow!("bad --listen: {e}"))?;
    }
    if !net_listen.is_empty() || !net_connect.is_empty() {
        grants = grants.grant_with(
            CapabilityId::net_v1(),
            net_grant_config(&net_listen, &net_connect),
        );
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
    // `--timeout-ms 0` means no wall-clock deadline at all (a server waiting in `accept`
    // must not be killed by it); so does leaving the flag out.
    limits.timeout = args
        .timeout_ms
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis);

    let bytes = std::fs::read(&args.module)
        .with_context(|| format!("cannot read {}", args.module.display()))?;
    let mut config = RuntimeConfig::default();
    config.engine.fuel_metering = args.fuel.is_some();
    let runtime = Runtime::new(config, registry())?;
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
    let has_orin_main = artifact
        .info()
        .exports
        .iter()
        .any(|e| e.name == ORIN_MAIN && e.kind == "func");
    let code = drive(&mut isolate, &args, has_start, has_orin_main).await?;
    isolate.terminate();
    Ok(code)
}

const ORIN_MAIN: &str = "orin_main";
const ORIN_EXIT_STATUS: &str = "orin_exit_status";

/// Initialises the isolate and runs `--invoke NAME`, or `_start` of a command module, or
/// `orin_main` of an Orin program (see `docs/host-abi.md` section 5).
async fn drive(
    isolate: &mut orbit_runtime::Isolate,
    args: &RunArgs,
    has_start: bool,
    has_orin_main: bool,
) -> Result<i32> {
    match isolate.init().await {
        Ok(()) => {}
        Err(ExecError::Exit(code)) => return Ok(code),
        Err(e) => return Err(e).context("initialisation failed"),
    }
    if let Some(code) = isolate.exit_code() {
        return Ok(code);
    }
    if args.invoke.is_none() && !has_start && has_orin_main {
        return run_orin_main(isolate).await;
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

/// Calls `orin_main` and returns its result as the exit status. After a trap, asks
/// `orin_exit_status` (when exported) for the status the program set; -1, a missing export or
/// a failing call mean a genuine trap, which is reported as the error.
async fn run_orin_main(isolate: &mut orbit_runtime::Isolate) -> Result<i32> {
    match isolate.invoke(ORIN_MAIN, &[]).await {
        Ok(results) => Ok(results
            .first()
            .map_or(0, |r| i32::try_from(*r).unwrap_or(1))),
        Err(ExecError::Exit(code)) => Ok(code),
        Err(e @ ExecError::Trap(_)) => match isolate.post_mortem_i32(ORIN_EXIT_STATUS).await {
            Some(status) if status != -1 => {
                eprintln!("error: `{ORIN_MAIN}` failed: {e}");
                Ok(status)
            }
            _ => Err(e).context(format!("`{ORIN_MAIN}` failed")),
        },
        Err(e) => Err(e).with_context(|| format!("`{ORIN_MAIN}` failed")),
    }
}
