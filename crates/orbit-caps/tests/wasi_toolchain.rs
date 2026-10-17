//! Programs built by real toolchains run on the WASI shim: Rust (`wasm32-wasip1`, fixture
//! `wasi_hello.wasm`, source in `tests/guests/wasi-hello`) and C (clang + wasi-libc, fixture
//! `wasi_c.wasm`, source in `tests/guests/wasi-c`). Rebuild both with `tests/guests/build.sh`.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use orbit_abi::CapabilityId;
use orbit_caps::{ManualClock, MemorySink, WasiCapability};
use orbit_runtime::{
    CapabilityRegistry, ExecError, Grants, Isolate, IsolateState, Limits, Runtime, RuntimeConfig,
};
use serde_json::json;

fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

struct Run {
    iso: Isolate,
    sink: MemorySink,
}

impl Run {
    /// Instantiates `wasm` with the WASI shim (and any extra grants) and runs `_initialize`.
    async fn new(wasm: &[u8], config: serde_json::Value, extra: &[CapabilityId]) -> Self {
        let sink = MemorySink::new();
        let clock = ManualClock::new(1_700_000_000_000_000_000);
        let rt = Runtime::new(
            RuntimeConfig::default(),
            CapabilityRegistry::new()
                .with(WasiCapability::new(sink.clone()).with_time_source(Arc::new(clock))),
        )
        .unwrap();
        let art = rt.load(wasm).unwrap();
        let mut grants = Grants::none().grant_with(CapabilityId::wasi_p1_v1(), config);
        for id in extra {
            grants = grants.grant(id.clone());
        }
        let mut iso = rt
            .spawn_isolate(&art, "toolchain", &grants, Limits::default())
            .await
            .unwrap();
        iso.init().await.unwrap();
        Self { iso, sink }
    }

    async fn start(&mut self) -> Result<Vec<i64>, ExecError> {
        self.iso.invoke("_start", &[]).await
    }

    fn out(&self) -> Vec<String> {
        self.sink
            .records()
            .iter()
            .filter(|r| r.level.as_str() == "info")
            .map(|r| r.message.clone())
            .collect()
    }

    fn err(&self) -> Vec<String> {
        self.sink
            .records()
            .iter()
            .filter(|r| r.level.as_str() == "warn")
            .map(|r| r.message.clone())
            .collect()
    }
}

fn args(list: &[&str]) -> serde_json::Value {
    let mut argv = vec!["main"];
    argv.extend(list);
    json!({ "args": argv })
}

// --- Rust ----------------------------------------------------------------------------------

#[test]
fn rust_fixture_imports_exactly_the_documented_functions() {
    let rt = Runtime::new(
        RuntimeConfig::default(),
        CapabilityRegistry::new().with(WasiCapability::new(MemorySink::new())),
    )
    .unwrap();
    let art = rt.load(&fixture("wasi_hello.wasm")).unwrap();
    let imports: BTreeSet<_> = art
        .info()
        .imports
        .iter()
        .map(|i| format!("{}.{}", i.module, i.name))
        .collect();
    let expected: BTreeSet<_> = [
        "args_get",
        "args_sizes_get",
        "clock_time_get",
        "environ_get",
        "environ_sizes_get",
        "fd_read",
        "fd_write",
        "proc_exit",
        "random_get",
    ]
    .iter()
    .map(|n| format!("wasi_snapshot_preview1.{n}"))
    .collect();
    assert_eq!(imports, expected);
    assert_eq!(
        art.required().iter().collect::<Vec<_>>(),
        vec![&CapabilityId::wasi_p1_v1()]
    );
}

#[tokio::test]
async fn rust_println_reaches_the_log_sink_and_args_reach_the_program() {
    let mut r = Run::new(&fixture("wasi_hello.wasm"), json!(null), &[]).await;
    r.start().await.unwrap();
    assert_eq!(r.out(), vec!["hello from wasi", r#"args=["main"]"#, "done"]);
    assert_eq!(r.err(), vec!["this goes to stderr"]);
    assert_eq!(r.iso.state(), IsolateState::Idle);
    assert_eq!(r.iso.exit_code(), None);

    let mut r = Run::new(&fixture("wasi_hello.wasm"), args(&["x", "y z"]), &[]).await;
    r.start().await.unwrap();
    assert_eq!(r.out()[1], r#"args=["main", "x", "y z"]"#);
}

#[tokio::test]
async fn rust_process_exit_propagates_the_code() {
    let mut r = Run::new(&fixture("wasi_hello.wasm"), args(&["exit"]), &[]).await;
    assert_eq!(r.start().await, Err(ExecError::Exit(3)));
    assert_eq!(r.iso.exit_code(), Some(3));
    assert_eq!(r.iso.state(), IsolateState::Failed);
    assert!(r.out().contains(&"exiting with 3".to_owned()));

    let mut r = Run::new(&fixture("wasi_hello.wasm"), args(&["exit", "0"]), &[]).await;
    r.start().await.unwrap();
    assert_eq!(r.iso.exit_code(), Some(0));
    assert_eq!(r.iso.state(), IsolateState::Terminated);
    assert_eq!(r.out().last().unwrap(), "exiting with 0");
}

#[tokio::test]
async fn rust_environment_comes_from_the_grant_only() {
    let mut r = Run::new(&fixture("wasi_hello.wasm"), args(&["env"]), &[]).await;
    r.start().await.unwrap();
    assert!(r.out().contains(&"env=[]".to_owned()), "{:?}", r.out());

    let cfg = json!({"args": ["main", "env"], "env": {"GREETING": "hi", "A": "1"}});
    let mut r = Run::new(&fixture("wasi_hello.wasm"), cfg, &[]).await;
    r.start().await.unwrap();
    assert!(
        r.out()
            .contains(&r#"env=[("A", "1"), ("GREETING", "hi")]"#.to_owned()),
        "{:?}",
        r.out()
    );
}

#[tokio::test]
async fn rust_system_time_needs_the_clock_grant() {
    // Without clock/v1: `clock_time_get` is ENOTCAPABLE, std panics, the guest aborts and the
    // isolate fails with a trap. The panic message still reaches the log sink.
    let mut r = Run::new(&fixture("wasi_hello.wasm"), args(&["time"]), &[]).await;
    let err = r.start().await.unwrap_err();
    assert!(matches!(err, ExecError::Trap(_)), "{err}");
    assert_eq!(r.iso.state(), IsolateState::Failed);
    let stderr = r.err().join("\n");
    assert!(stderr.contains("panicked"), "{stderr}");
    assert!(stderr.contains("Capabilities insufficient"), "{stderr}");
    assert!(!r.out().iter().any(|l| l.starts_with("unix_secs")));

    let mut r = Run::new(
        &fixture("wasi_hello.wasm"),
        args(&["time"]),
        &[CapabilityId::clock_v1()],
    )
    .await;
    r.start().await.unwrap();
    assert!(
        r.out().contains(&"unix_secs=1700000000".to_owned()),
        "{:?}",
        r.out()
    );
    assert!(r.out().contains(&"monotonic_ok=true".to_owned()));
}

#[tokio::test]
async fn rust_hashmap_needs_the_random_grant() {
    let mut r = Run::new(&fixture("wasi_hello.wasm"), args(&["random"]), &[]).await;
    assert!(matches!(r.start().await, Err(ExecError::Trap(_))));
    assert!(r.err().join("\n").contains("panicked"));

    let mut r = Run::new(
        &fixture("wasi_hello.wasm"),
        args(&["random"]),
        &[CapabilityId::random_v1()],
    )
    .await;
    r.start().await.unwrap();
    assert!(r.out().contains(&"hashmap_ok=true".to_owned()));
}

#[tokio::test]
async fn rust_stdin_is_empty_and_panics_are_controlled() {
    let mut r = Run::new(&fixture("wasi_hello.wasm"), args(&["stdin"]), &[]).await;
    r.start().await.unwrap();
    assert!(r.out().contains(&"stdin_bytes=0".to_owned()));

    let mut r = Run::new(&fixture("wasi_hello.wasm"), args(&["panic"]), &[]).await;
    assert!(matches!(r.start().await, Err(ExecError::Trap(_))));
    assert!(r.err().join("\n").contains("boom"));
    assert_eq!(r.iso.state(), IsolateState::Failed);
}

#[tokio::test]
async fn rust_guest_is_bounded_by_fuel() {
    let sink = MemorySink::new();
    let rt = Runtime::new(
        RuntimeConfig::default(),
        CapabilityRegistry::new().with(WasiCapability::new(sink)),
    )
    .unwrap();
    let art = rt.load(&fixture("wasi_hello.wasm")).unwrap();
    let grants = Grants::none().grant_with(
        CapabilityId::wasi_p1_v1(),
        json!({"args": ["main", "spin"]}),
    );
    // Well above what instantiation can cost on any platform (data segments are charged
    // without a copy-on-write image), far below what the endless loop needs.
    let mut iso = rt
        .spawn_isolate(&art, "t", &grants, Limits::default().with_fuel(20_000_000))
        .await
        .unwrap();
    iso.init().await.unwrap();
    assert_eq!(iso.invoke("_start", &[]).await, Err(ExecError::OutOfFuel));
}

// --- C -------------------------------------------------------------------------------------

#[tokio::test]
async fn c_program_runs_with_args_env_exit_code_and_clock() {
    let wasm = fixture("wasi_c.wasm");
    let cfg = json!({"args": ["main", "t"], "env": {"GREETING": "yo"}});
    let mut r = Run::new(&wasm, cfg, &[CapabilityId::clock_v1()]).await;
    r.start().await.unwrap();
    assert_eq!(
        r.out(),
        vec![
            "c hello argc=2",
            "argv[0]=main",
            "argv[1]=t",
            "GREETING=yo",
            "time=1700000000"
        ]
    );

    let mut r = Run::new(&wasm, args(&["e", "4"]), &[]).await;
    assert_eq!(r.start().await, Err(ExecError::Exit(4)));
    assert_eq!(r.out().last().unwrap(), "GREETING=(unset)");

    // `time()` without the clock grant: wasi-libc gets ENOTCAPABLE and does not crash.
    let mut r = Run::new(&wasm, args(&["t"]), &[]).await;
    r.start().await.unwrap();
}
