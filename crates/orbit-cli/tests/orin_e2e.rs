//! End-to-end: real Orin programs, compiled by the Orin compiler (`orin build --target=wasm32-orbit` / `wasm64-orbit`,
//! see tests/guests/orin/build.sh and fixtures/orin/VERSION), run in Orbit through the runtime API and the `orbit` CLI.
//! The fixtures are committed, so no Orin toolchain is needed here. `<name>.expected` is the stdout of the same program
//! built natively by Orin.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::Arc;

use orbit_abi::CapabilityId;
use orbit_caps::{ClockCapability, LogCapability, LogLevel, MemorySink};
use orbit_runtime::{
    Artifact, CapabilityRegistry, ExecError, Grants, InstantiateError, Isolate, IsolateState,
    Limits, Runtime, RuntimeConfig,
};

const WIDTHS: [&str; 2] = ["32", "64"];

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/orin")
}

fn wasm_path(name: &str, w: &str) -> String {
    dir()
        .join(format!("{name}_{w}.wasm"))
        .to_str()
        .unwrap()
        .to_owned()
}

fn expected_lines(name: &str) -> Vec<String> {
    std::fs::read_to_string(dir().join(format!("{name}.expected")))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn setup() -> (Runtime, MemorySink) {
    let sink = MemorySink::new();
    let registry = CapabilityRegistry::new()
        .with(LogCapability::new(sink.clone()))
        .with(ClockCapability::system());
    (
        Runtime::new(RuntimeConfig::default(), registry).unwrap(),
        sink,
    )
}

fn load(rt: &Runtime, name: &str, w: &str) -> Arc<Artifact> {
    let bytes = std::fs::read(wasm_path(name, w)).unwrap();
    rt.load(&bytes)
        .unwrap_or_else(|e| panic!("{name}_{w}: {e}"))
}

fn grants(caps: &[CapabilityId]) -> Grants {
    caps.iter().fold(Grants::none(), |g, c| g.grant(c.clone()))
}

fn log_only() -> Grants {
    grants(&[CapabilityId::log_v1()])
}

fn log_and_clock() -> Grants {
    grants(&[CapabilityId::log_v1(), CapabilityId::clock_v1()])
}

async fn spawn(rt: &Runtime, art: &Arc<Artifact>, g: &Grants) -> Isolate {
    spawn_with(rt, art, g, Limits::default()).await
}

async fn spawn_with(rt: &Runtime, art: &Arc<Artifact>, g: &Grants, limits: Limits) -> Isolate {
    let mut iso = rt
        .spawn_isolate(art, "orin", g, limits)
        .await
        .expect("instantiates");
    iso.init().await.expect("init");
    iso
}

/// Runs `orin_main` the way `orbit run` does. Ok = its result; Err = the trap, with the status from `orin_exit_status`.
async fn run_main(iso: &mut Isolate) -> Result<i32, (ExecError, Option<i32>)> {
    match iso.invoke("orin_main", &[]).await {
        Ok(r) => Ok(i32::try_from(r[0]).unwrap()),
        Err(e) => {
            let status = iso.post_mortem_i32("orin_exit_status").await;
            Err((e, status))
        }
    }
}

fn lines(sink: &MemorySink) -> Vec<(LogLevel, String)> {
    sink.records()
        .into_iter()
        .map(|r| (r.level, r.message))
        .collect()
}

fn info_lines(sink: &MemorySink) -> Vec<String> {
    lines(sink)
        .into_iter()
        .filter(|(l, _)| *l == LogLevel::Info)
        .map(|(_, m)| m)
        .collect()
}

fn module_names(art: &Artifact) -> Vec<String> {
    let mut v: Vec<String> = art
        .info()
        .imports
        .iter()
        .map(|i| i.module.clone())
        .collect();
    v.sort();
    v.dedup();
    v
}

// ---------------------------------------------------------------- inspect-level facts

#[test]
fn every_fixture_has_the_agreed_shape() {
    let (rt, _) = setup();
    // (program, capabilities the manifest must list, capabilities that must be imported)
    let cases: [(&str, &[&str], &[&str]); 6] = [
        ("hello", &["log/v1"], &["orbit:log/v1"]),
        ("exit", &["log/v1"], &["orbit:log/v1"]),
        ("trap", &["log/v1"], &["orbit:log/v1"]),
        ("compute", &["log/v1"], &["orbit:log/v1"]),
        (
            "clock",
            &["clock/v1", "log/v1"],
            &["orbit:clock/v1", "orbit:log/v1"],
        ),
        // The spawned closures are unresolved callbacks: the manifest conservatively lists clock/v1 (contract 2a).
        ("tasks", &["clock/v1", "log/v1"], &["orbit:log/v1"]),
    ];
    for w in WIDTHS {
        for (name, requires, imports) in cases {
            let art = load(&rt, name, w);
            let what = format!("{name}_{w}");
            // imports: only orbit:log/v1 and orbit:clock/v1, no WASI
            let mods = module_names(&art);
            assert_eq!(mods, imports, "{what} imports");
            assert!(
                art.info()
                    .imports
                    .iter()
                    .all(|i| i.module == "orbit:log/v1" || i.module == "orbit:clock/v1"),
                "{what}"
            );
            // manifest
            let m = art
                .manifest()
                .unwrap_or_else(|| panic!("{what}: no manifest"));
            assert_eq!(m.abi, 1, "{what}");
            assert_eq!(m.producer.as_deref(), Some("orin"), "{what}");
            let mut got = m.requires.clone();
            got.sort();
            assert_eq!(got, requires, "{what} requires");
            // exports
            for (export, kind) in [
                ("memory", "memory"),
                ("orin_main", "func"),
                ("orin_exit_status", "func"),
            ] {
                assert!(
                    art.info()
                        .exports
                        .iter()
                        .any(|e| e.name == export && e.kind == kind),
                    "{what}: export {export}"
                );
            }
            assert!(
                !art.info().exports.iter().any(|e| e.name == "_start"),
                "{what}: no _start"
            );
            // memory width
            let wide = art
                .module()
                .exports()
                .find(|e| e.name() == "memory")
                .and_then(|e| e.ty().memory().map(|t| t.is_64()))
                .unwrap();
            assert_eq!(wide, w == "64", "{what}: memory width");
            // required = manifest requires + imports
            for r in requires {
                assert!(
                    art.required().contains(&r.parse::<CapabilityId>().unwrap()),
                    "{what} {r}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------- run through the runtime API

#[tokio::test]
async fn hello_logs_in_order_and_exits_0() {
    for w in WIDTHS {
        let (rt, sink) = setup();
        let art = load(&rt, "hello", w);
        let mut iso = spawn(&rt, &art, &log_only()).await;
        assert_eq!(run_main(&mut iso).await.unwrap(), 0);
        assert_eq!(info_lines(&sink), expected_lines("hello"), "hello_{w}");
        assert_eq!(
            info_lines(&sink),
            ["hello from orin", "line two", "line three"]
        );
        assert_eq!(lines(&sink).len(), 3, "only info records");
    }
}

#[tokio::test]
async fn exit_7_is_reported_through_orin_exit_status() {
    for w in WIDTHS {
        let (rt, sink) = setup();
        let art = load(&rt, "exit", w);
        let mut iso = spawn(&rt, &art, &log_only()).await;
        let (err, status) = run_main(&mut iso).await.unwrap_err();
        assert!(matches!(err, ExecError::Trap(_)), "exit_{w}: {err}");
        assert_eq!(status, Some(7), "exit_{w}");
        assert_eq!(iso.state(), IsolateState::Failed);
        assert_eq!(info_lines(&sink), expected_lines("exit"));
        assert_eq!(info_lines(&sink), ["about to exit"]);
    }
}

#[tokio::test]
async fn out_of_bounds_index_traps_with_status_101() {
    for w in WIDTHS {
        let (rt, sink) = setup();
        let art = load(&rt, "trap", w);
        let mut iso = spawn(&rt, &art, &log_only()).await;
        let (err, status) = run_main(&mut iso).await.unwrap_err();
        assert!(matches!(err, ExecError::Trap(_)), "trap_{w}: {err}");
        assert_eq!(status, Some(101), "trap_{w}");
        assert_eq!(iso.state(), IsolateState::Failed);
        // stdout before the trap, then the trap message on the error stream
        assert_eq!(info_lines(&sink), expected_lines("trap"));
        let errs: Vec<String> = lines(&sink)
            .into_iter()
            .filter(|(l, _)| *l == LogLevel::Error)
            .map(|(_, m)| m)
            .collect();
        assert!(
            !errs.is_empty(),
            "trap_{w}: the trap message reaches the log"
        );
        eprintln!("trap_{w} error records: {errs:?}");
    }
}

#[tokio::test]
async fn compute_checksums_match_the_native_orin_build() {
    for w in WIDTHS {
        let (rt, sink) = setup();
        let art = load(&rt, "compute", w);
        // Orin ints are 8 bytes: the sieve over 5M flags needs well over the 16 MiB default.
        let limits = Limits::default().with_max_memory(256 << 20);
        let mut iso = spawn_with(&rt, &art, &log_only(), limits).await;
        assert_eq!(run_main(&mut iso).await.unwrap(), 0);
        let got = info_lines(&sink);
        assert_eq!(got, expected_lines("compute"), "compute_{w}");
        // The values recorded from the native build (tests/guests/orin/build.sh writes compute.expected).
        assert_eq!(
            got,
            [
                "fib(35)=9227465",
                "primes<=5000000: 348513",
                "collatz(1..300000) steps: 35669725",
                "words: 1000 last w81",
            ]
        );
        assert!(iso.metrics().invocations >= 1);
    }
}

#[tokio::test]
async fn compute_hits_the_default_memory_limit() {
    for w in WIDTHS {
        let (rt, sink) = setup();
        let art = load(&rt, "compute", w);
        let mut iso = spawn(&rt, &art, &log_only()).await;
        let (err, status) = run_main(&mut iso).await.unwrap_err();
        // The guest sees memory.grow fail, reports `out of memory` and traps; Orbit classifies it as a memory-limit failure.
        assert!(
            matches!(err, ExecError::MemoryLimit { .. }),
            "compute_{w}: {err}"
        );
        assert_eq!(status, None, "no post-mortem after a memory-limit trap");
        assert_eq!(info_lines(&sink), ["fib(35)=9227465"]);
        assert!(
            lines(&sink)
                .iter()
                .any(|(l, m)| *l == LogLevel::Error && m.contains("out of memory")),
            "{:?}",
            lines(&sink)
        );
    }
}

#[tokio::test]
async fn clock_needs_clock_v1() {
    for w in WIDTHS {
        let (rt, sink) = setup();
        let art = load(&rt, "clock", w);
        // log only: refused at instantiation, nothing ran
        let e = rt
            .spawn_isolate(&art, "orin", &log_only(), Limits::default())
            .await
            .unwrap_err();
        assert!(
            matches!(e, InstantiateError::NotGranted(ref id) if *id == CapabilityId::clock_v1()),
            "clock_{w}: {e}"
        );
        assert!(sink.records().is_empty());
        // both granted: runs
        let mut iso = spawn(&rt, &art, &log_and_clock()).await;
        assert_eq!(run_main(&mut iso).await.unwrap(), 0);
        assert_eq!(info_lines(&sink), expected_lines("clock"));
    }
}

#[tokio::test]
async fn tasks_run_and_join_deterministically() {
    for w in WIDTHS {
        let (rt, sink) = setup();
        let art = load(&rt, "tasks", w);
        // The manifest lists clock/v1 (unresolved closures), so a log-only grant is refused although the module never imports the clock.
        let e = rt
            .spawn_isolate(&art, "orin", &log_only(), Limits::default())
            .await
            .unwrap_err();
        assert!(
            matches!(e, InstantiateError::NotGranted(ref id) if *id == CapabilityId::clock_v1()),
            "tasks_{w}: {e}"
        );
        let mut iso = spawn(&rt, &art, &log_and_clock()).await;
        assert_eq!(run_main(&mut iso).await.unwrap(), 0);
        assert_eq!(info_lines(&sink), expected_lines("tasks"));
        assert_eq!(info_lines(&sink), ["joined: 42", "channel sum: 55"]);
    }
}

#[tokio::test]
async fn without_a_log_grant_nothing_runs() {
    for w in WIDTHS {
        for name in ["hello", "exit", "trap", "compute"] {
            let (rt, sink) = setup();
            let art = load(&rt, name, w);
            let e = rt
                .spawn_isolate(&art, "orin", &Grants::none(), Limits::default())
                .await
                .unwrap_err();
            assert!(
                matches!(e, InstantiateError::NotGranted(ref id) if *id == CapabilityId::log_v1()),
                "{name}_{w}: {e}"
            );
            assert!(sink.records().is_empty());
            assert_eq!(rt.reserved_memory_bytes(), 0);
        }
    }
}

#[tokio::test]
async fn one_runtime_runs_both_widths_isolated() {
    let (rt, sink) = setup();
    let a32 = load(&rt, "hello", "32");
    let a64 = load(&rt, "hello", "64");
    let mut i32_ = spawn(&rt, &a32, &log_only()).await;
    let mut i64_ = spawn(&rt, &a64, &log_only()).await;
    assert_eq!(run_main(&mut i32_).await.unwrap(), 0);
    assert_eq!(run_main(&mut i64_).await.unwrap(), 0);
    assert_eq!(sink.records().len(), 6);
    // a second run of the same instance works (the module keeps its state; hello is stateless)
    assert_eq!(run_main(&mut i64_).await.unwrap(), 0);
}

// ---------------------------------------------------------------- the orbit CLI

fn orbit(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_orbit"))
        .args(args)
        .output()
        .expect("orbit runs")
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// The guest's log records as printed by the CLI: `[info] cli/i-1: <message>`.
fn cli_info(o: &Output) -> Vec<String> {
    stderr(o)
        .lines()
        .filter_map(|l| l.strip_prefix("[info] cli/i-1: "))
        .map(str::to_owned)
        .collect()
}

#[test]
fn cli_inspect_shows_the_orin_contract() {
    for w in WIDTHS {
        let o = orbit(&["inspect", &wasm_path("clock", w)]);
        assert!(o.status.success(), "{}", stderr(&o));
        let out = String::from_utf8_lossy(&o.stdout);
        assert!(out.contains("orbit:log/v1.write func"), "{out}");
        assert!(out.contains("orbit:clock/v1."), "{out}");
        assert!(out.contains("orin_main func"), "{out}");
        assert!(out.contains("orin_exit_status func"), "{out}");
        assert!(out.contains("memory memory"), "{out}");
        assert!(out.contains("producer orin"), "{out}");
        assert!(out.contains("requires: clock/v1, log/v1"), "{out}");
        assert!(out.contains("validation: ok"), "{out}");
        assert!(!out.contains("wasi_snapshot_preview1"), "{out}");
    }
}

#[test]
fn cli_run_hello_and_compute() {
    for w in WIDTHS {
        for name in ["hello", "compute"] {
            let o = orbit(&[
                "run",
                &wasm_path(name, w),
                "--grant",
                "log/v1",
                "--max-memory",
                "268435456",
            ]);
            assert_eq!(o.status.code(), Some(0), "{name}_{w}: {}", stderr(&o));
            assert_eq!(cli_info(&o), expected_lines(name), "{name}_{w}");
            assert!(o.stdout.is_empty(), "stdout carries no guest output");
        }
    }
}

#[test]
fn cli_run_compute_under_the_default_memory_limit_fails() {
    for w in WIDTHS {
        let o = orbit(&["run", &wasm_path("compute", w), "--grant", "log/v1"]);
        assert_eq!(o.status.code(), Some(1), "compute_{w}: {}", stderr(&o));
        assert!(stderr(&o).contains("memory limit"), "{}", stderr(&o));
        assert!(stderr(&o).contains("out of memory"), "{}", stderr(&o));
    }
}

#[test]
fn cli_run_exit_codes() {
    for w in WIDTHS {
        let o = orbit(&["run", &wasm_path("exit", w), "--grant", "log/v1"]);
        assert_eq!(o.status.code(), Some(7), "exit_{w}: {}", stderr(&o));
        assert_eq!(cli_info(&o), ["about to exit"]);
        assert!(stderr(&o).contains("orin_main"), "{}", stderr(&o));

        let o = orbit(&["run", &wasm_path("trap", w), "--grant", "log/v1"]);
        assert_eq!(o.status.code(), Some(101), "trap_{w}: {}", stderr(&o));
        assert_eq!(cli_info(&o), ["before the trap"]);
        assert!(
            stderr(&o).contains("[error]"),
            "trap message logged: {}",
            stderr(&o)
        );
    }
}

#[test]
fn cli_run_clock_and_tasks_need_clock_grant() {
    for w in WIDTHS {
        for name in ["clock", "tasks"] {
            let o = orbit(&["run", &wasm_path(name, w), "--grant", "log/v1"]);
            assert!(!o.status.success());
            assert!(
                stderr(&o).contains("capability not granted: clock/v1"),
                "{name}_{w}: {}",
                stderr(&o)
            );
            assert!(cli_info(&o).is_empty());
            let o = orbit(&[
                "run",
                &wasm_path(name, w),
                "--grant",
                "log/v1",
                "--grant",
                "clock/v1",
            ]);
            assert_eq!(o.status.code(), Some(0), "{name}_{w}: {}", stderr(&o));
            assert_eq!(cli_info(&o), expected_lines(name), "{name}_{w}");
        }
    }
}

#[test]
fn cli_run_without_log_grant_is_refused_before_running() {
    for w in WIDTHS {
        let o = orbit(&["run", &wasm_path("hello", w)]);
        assert!(!o.status.success());
        assert!(
            stderr(&o).contains("capability not granted: log/v1"),
            "{}",
            stderr(&o)
        );
        assert!(cli_info(&o).is_empty());
    }
}

#[test]
fn cli_run_with_fuel_metering_still_matches() {
    for w in WIDTHS {
        let o = orbit(&[
            "run",
            &wasm_path("hello", w),
            "--grant",
            "log/v1",
            "--fuel",
            "100000000",
        ]);
        assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
        assert_eq!(cli_info(&o), expected_lines("hello"));
    }
}
