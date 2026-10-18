//! `orbit:thread/v1`: real clang-built guests (tests/guests/thread) on a shared memory.

use std::sync::Arc;
use std::time::{Duration, Instant};

use orbit_abi::CapabilityId;
use orbit_caps::{LogCapability, MemorySink, NetCapability, net_grant_config};
use orbit_runtime::{
    Artifact, CapabilityRegistry, ExecError, Grants, InstantiateError, Isolate, IsolateState,
    LoadError, Limits, PoolConfig, ReusePolicy, Runtime, RuntimeConfig, ThreadCapability,
};

const THREADS: &[u8] = include_bytes!("fixtures/threads/threads.wasm");
const NET_THREADS: &[u8] = include_bytes!("fixtures/threads/net_threads.wasm");

fn runtime_with(config: RuntimeConfig) -> (Runtime, MemorySink) {
    let sink = MemorySink::new();
    let registry = CapabilityRegistry::new()
        .with(LogCapability::new(sink.clone()))
        .with(ThreadCapability::new())
        .with(NetCapability::new());
    (Runtime::new(config, registry).expect("runtime"), sink)
}

fn runtime() -> (Runtime, MemorySink) {
    runtime_with(RuntimeConfig::default())
}

fn grants() -> Grants {
    Grants::none()
        .grant(CapabilityId::log_v1())
        .grant(CapabilityId::thread_v1())
}

fn grants_max(n: u64) -> Grants {
    Grants::none()
        .grant(CapabilityId::log_v1())
        .grant_with(
            CapabilityId::thread_v1(),
            serde_json::json!({ "max_threads": n }),
        )
}

async fn ready(rt: &Runtime, art: &Arc<Artifact>, g: &Grants, limits: Limits) -> Isolate {
    let mut iso = rt.spawn_isolate(art, "t", g, limits).await.expect("spawn");
    iso.init().await.expect("init");
    iso
}

fn assert_quick(start: Instant, what: &str) {
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "{what}: took {:?}",
        start.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn four_threads_count_exactly() {
    let (rt, _) = runtime();
    let art = rt.load(THREADS).expect("load");
    assert!(art.shared_memory().is_some());
    let mut iso = ready(&rt, &art, &grants(), Limits::default()).await;
    let r = iso.invoke("count", &[4, 100_000]).await.expect("count");
    assert_eq!(r, vec![400_000]);
    assert_eq!(iso.state(), IsolateState::Idle);
    assert_eq!(iso.metrics().threads_spawned, 4);
    // The isolate runs again: a second run on the same memory keeps the counter.
    let r = iso.invoke("count", &[2, 10]).await.expect("count again");
    assert_eq!(r, vec![400_020]);
}

#[tokio::test(flavor = "current_thread")]
async fn runs_on_a_current_thread_runtime() {
    let (rt, _) = runtime();
    let art = rt.load(THREADS).unwrap();
    let mut iso = ready(&rt, &art, &grants(), Limits::default()).await;
    assert_eq!(
        iso.invoke("count", &[3, 1000]).await.unwrap(),
        vec![3000]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wait_and_notify_between_threads() {
    let (rt, _) = runtime();
    let art = rt.load(THREADS).unwrap();
    let mut iso = ready(&rt, &art, &grants(), Limits::default()).await;
    let start = Instant::now();
    assert_eq!(iso.invoke("wait_notify", &[]).await.unwrap(), vec![42]);
    assert_quick(start, "wait_notify");
}

#[tokio::test]
async fn spawn_needs_a_grant() {
    let (rt, _) = runtime();
    let art = rt.load(THREADS).unwrap();
    let only_log = Grants::none().grant(CapabilityId::log_v1());
    let err = rt
        .spawn_isolate(&art, "t", &only_log, Limits::default())
        .await
        .unwrap_err();
    assert!(
        matches!(err, InstantiateError::NotGranted(ref id) if *id == CapabilityId::thread_v1()),
        "{err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_quota() {
    let (rt, _) = runtime();
    let art = rt.load(THREADS).unwrap();
    let mut iso = ready(&rt, &art, &grants_max(2), Limits::default()).await;
    // Four spawns while the first two are still blocked: two succeed, two get ERR_QUOTA.
    assert_eq!(iso.invoke("spawn_n", &[4]).await.unwrap(), vec![2]);
    assert_eq!(
        iso.invoke("last_status", &[]).await.unwrap(),
        vec![i64::from(orbit_abi::status::ERR_QUOTA)]
    );
    // max_threads = 0 denies.
    let mut iso0 = ready(&rt, &art, &grants_max(0), Limits::default()).await;
    assert_eq!(iso0.invoke("spawn_n", &[1]).await.unwrap(), vec![0]);
    assert_eq!(
        iso0.invoke("last_status", &[]).await.unwrap(),
        vec![i64::from(orbit_abi::status::ERR_DENIED)]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_trap_in_one_thread_ends_all_including_blocked_waits() {
    let (rt, _) = runtime();
    let art = rt.load(THREADS).unwrap();
    let mut iso = ready(&rt, &art, &grants(), Limits::default()).await;
    let start = Instant::now();
    let err = iso.invoke("trap_with_blockers", &[]).await.unwrap_err();
    assert_quick(start, "trap_with_blockers");
    match &err {
        ExecError::Trap(m) => assert!(m.contains("thread 3"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(iso.state(), IsolateState::Failed);
    assert!(matches!(iso.failure(), Some(ExecError::Trap(_))));
    iso.terminate();
}

#[tokio::test(flavor = "multi_thread")]
async fn exit_from_a_non_main_thread() {
    let (rt, _) = runtime();
    let art = rt.load(THREADS).unwrap();
    let mut iso = ready(&rt, &art, &grants(), Limits::default()).await;
    let start = Instant::now();
    let err = iso.invoke("exit_in_thread", &[]).await.unwrap_err();
    assert_quick(start, "exit_in_thread");
    assert!(matches!(err, ExecError::Trap(_)), "{err:?}");
    // The status was written into shared memory by the thread; the export of the main
    // instance reads it after the trap, like Orin's `orin_exit_status`.
    assert_eq!(iso.post_mortem_i32("orin_exit_status").await, Some(7));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_deadline_ends_a_spinning_thread_and_a_blocked_main() {
    let (rt, _) = runtime();
    let art = rt.load(THREADS).unwrap();
    let limits = Limits::default().with_timeout(Duration::from_millis(300));
    let mut iso = ready(&rt, &art, &grants(), limits).await;
    let start = Instant::now();
    let err = iso.invoke("spin_thread", &[]).await.unwrap_err();
    assert_eq!(err, ExecError::DeadlineExceeded);
    assert!(start.elapsed() >= Duration::from_millis(250));
    assert_quick(start, "spin_thread");
}

#[tokio::test(flavor = "multi_thread")]
async fn leftover_threads_are_terminated_when_main_returns() {
    let (rt, _) = runtime();
    let art = rt.load(THREADS).unwrap();
    let mut iso = ready(&rt, &art, &grants(), Limits::default()).await;
    let start = Instant::now();
    assert_eq!(iso.invoke("leave_thread_behind", &[]).await.unwrap(), vec![5]);
    assert_quick(start, "leave_thread_behind");
    assert_eq!(iso.state(), IsolateState::Idle);
    assert_eq!(iso.invoke("peek", &[]).await.unwrap(), vec![0]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_memory_limit_applies_to_the_shared_memory() {
    let (rt, _) = runtime();
    let art = rt.load(THREADS).unwrap();
    let decl = art.shared_memory().unwrap();
    assert_eq!(decl.max_pages, 65_536);
    // 64 pages = 4 MiB.
    let limits = Limits::default().with_max_memory(64 * 65_536);
    let mut iso = ready(&rt, &art, &grants(), limits).await;
    let start = iso.invoke("size", &[]).await.unwrap()[0];
    assert!(start >= 1 && start <= 64, "{start}");
    assert_eq!(iso.invoke("grow", &[10_000]).await.unwrap(), vec![-1]);
    assert_eq!(iso.invoke("grow", &[1]).await.unwrap(), vec![start]);
    assert_eq!(iso.invoke("grow", &[64 - start]).await.unwrap(), vec![-1]);
    assert_eq!(iso.metrics().memory_bytes, (start as u64 + 1) * 65_536);

    // A limit below the module's initial memory is refused at spawn.
    let tiny = Limits::default().with_max_memory(65_536);
    let err = rt.spawn_isolate(&art, "t", &grants(), tiny).await.unwrap_err();
    assert!(matches!(err, InstantiateError::ResourceLimit(_)), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn isolates_of_one_module_share_nothing() {
    let (rt, _) = runtime();
    let art = rt.load(THREADS).unwrap();
    let mut a = ready(&rt, &art, &grants(), Limits::default()).await;
    let mut b = ready(&rt, &art, &grants(), Limits::default()).await;
    a.invoke("poke", &[111]).await.unwrap();
    assert_eq!(b.invoke("peek", &[]).await.unwrap(), vec![0]);
    b.invoke("poke", &[222]).await.unwrap();
    assert_eq!(a.invoke("peek", &[]).await.unwrap(), vec![111]);
    assert_eq!(b.invoke("peek", &[]).await.unwrap(), vec![222]);
    // Threads of a and b run at the same time without crossing either.
    let (ra, rb) = tokio::join!(a.invoke("count", &[2, 5000]), b.invoke("count", &[3, 5000]));
    assert_eq!(ra.unwrap(), vec![10_000]);
    assert_eq!(rb.unwrap(), vec![15_000]);
}

#[tokio::test(flavor = "multi_thread")]
async fn host_functions_work_through_the_shared_memory_in_every_thread() {
    let (rt, sink) = runtime();
    let art = rt.load(THREADS).unwrap();
    let mut iso = ready(&rt, &art, &grants(), Limits::default()).await;
    assert_eq!(iso.invoke("log_main", &[]).await.unwrap(), vec![0]);
    iso.invoke("log_thread", &[]).await.unwrap();
    let recs = sink.records();
    assert_eq!(recs.len(), 2);
    assert!(recs.iter().all(|r| r.message == "hello from a guest thread"));
    let m = iso.metrics();
    assert_eq!(m.bytes_logged, 2 * 25);
    assert_eq!(m.host_calls[&CapabilityId::log_v1()], 2);
    assert_eq!(m.host_calls[&CapabilityId::thread_v1()], 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_log_budget_is_shared_by_all_threads() {
    let (rt, sink) = runtime();
    let art = rt.load(THREADS).unwrap();
    // Room for exactly one 25-byte message in total.
    let limits = Limits::default().with_max_log_bytes(40);
    let mut iso = ready(&rt, &art, &grants(), limits).await;
    iso.invoke("log_both", &[]).await.unwrap();
    assert_eq!(sink.records().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn net_handles_opened_in_one_thread_work_in_another() {
    let (rt, _) = runtime();
    let art = rt.load(NET_THREADS).unwrap();
    let g = Grants::none()
        .grant(CapabilityId::thread_v1())
        .grant_with(
            CapabilityId::net_v1(),
            net_grant_config(
                &["127.0.0.1:0-65535".to_owned()],
                &["127.0.0.1:1-65535".to_owned()],
            ),
        );
    let mut iso = ready(&rt, &art, &g, Limits::default()).await;
    let start = Instant::now();
    assert_eq!(iso.invoke("serve_in_thread", &[]).await.unwrap(), vec![4]);
    assert_quick(start, "serve_in_thread");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_thread_blocked_in_accept_ends_with_the_isolate() {
    // The thread waits 10 s in accept; a deadline of 300 ms must end it long before.
    let (rt, _) = runtime();
    let art = rt.load(NET_THREADS).unwrap();
    let g = Grants::none().grant(CapabilityId::thread_v1()).grant_with(
        CapabilityId::net_v1(),
        net_grant_config(&["127.0.0.1:0-65535".to_owned()], &[]),
    );
    // serve_in_thread connects, which is denied here: main returns -3 and the thread, still
    // in accept, must be terminated rather than waited for.
    let mut iso = ready(&rt, &art, &g, Limits::default()).await;
    let start = Instant::now();
    assert_eq!(iso.invoke("serve_in_thread", &[]).await.unwrap(), vec![-3]);
    assert!(start.elapsed() < Duration::from_secs(5), "{:?}", start.elapsed());
}

#[tokio::test(flavor = "multi_thread")]
async fn fuel_is_a_budget_per_thread() {
    let mut cfg = RuntimeConfig::default();
    cfg.engine.fuel_metering = true;
    let (rt, _) = runtime_with(cfg);
    let art = rt.load(THREADS).unwrap();
    let limits = Limits::default().with_fuel(5_000_000);
    let mut iso = ready(&rt, &art, &grants(), limits).await;
    assert_eq!(iso.invoke("count", &[2, 1000]).await.unwrap(), vec![2000]);
    let used = iso.metrics().last_invocation.unwrap().fuel_consumed.unwrap();
    assert!(used > 2 * 1000, "{used}");
    // 3 000 000 iterations in one thread exceed its own 5M budget: the thread runs dry and
    // takes the whole isolate with it.
    let err = iso.invoke("count", &[1, 3_000_000]).await.unwrap_err();
    assert_eq!(err, ExecError::OutOfFuel);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pool_for_threaded_modules_is_fresh_per_acquire_only() {
    let (rt, _) = runtime();
    let art = rt.load(THREADS).unwrap();
    let reuse = PoolConfig::default().with_reuse(ReusePolicy::Reuse { max_uses: 5 });
    let err = rt
        .pool(&art, "t", &grants(), Limits::default(), reuse)
        .err()
        .expect("reuse refused");
    assert!(matches!(err, InstantiateError::ThreadsCannotBeReused), "{err}");
    let pool = rt
        .pool(&art, "t", &grants(), Limits::default(), PoolConfig::default())
        .expect("fresh pool");
    for _ in 0..2 {
        let mut iso = pool.acquire().await.unwrap();
        assert_eq!(iso.invoke("count", &[2, 100]).await.unwrap(), vec![200]);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_an_invocation_stops_its_threads() {
    let (rt, _) = runtime();
    let art = rt.load(THREADS).unwrap();
    let mut iso = ready(&rt, &art, &grants(), Limits::default()).await;
    let r = tokio::time::timeout(Duration::from_millis(300), iso.invoke("spin_thread", &[])).await;
    assert!(r.is_err(), "no deadline, so the call is still running");
    drop(iso); // terminates the run; the spinning thread must stop
    tokio::time::sleep(Duration::from_millis(200)).await;
}

// --- modules without threads, and rejected shapes -------------------------------------------

const UNSHARED: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "bump") (result i32)
    (i32.store (i32.const 0) (i32.add (i32.load (i32.const 0)) (i32.const 10)))
    (i32.load (i32.const 0))))
"#;

#[tokio::test]
async fn an_unshared_memory_module_still_runs() {
    let (rt, _) = runtime();
    let art = rt.load(UNSHARED.as_bytes()).unwrap();
    assert!(art.shared_memory().is_none());
    let mut iso = ready(&rt, &art, &Grants::none(), Limits::default()).await;
    assert_eq!(iso.invoke("bump", &[]).await.unwrap(), vec![10]);
    assert_eq!(iso.invoke("bump", &[]).await.unwrap(), vec![20]);
}

const SHARED_NO_THREADS: &str = r#"
(module
  (import "env" "memory" (memory 1 4 shared))
  (func (export "bump") (result i32)
    (i32.atomic.rmw.add (i32.const 0) (i32.const 3))))
"#;

#[tokio::test]
async fn a_shared_memory_module_without_spawn_needs_no_grant() {
    let (rt, _) = runtime();
    let art = rt.load(SHARED_NO_THREADS.as_bytes()).unwrap();
    let mut iso = ready(&rt, &art, &Grants::none(), Limits::default()).await;
    assert_eq!(iso.invoke("bump", &[]).await.unwrap(), vec![0]);
    assert_eq!(iso.invoke("bump", &[]).await.unwrap(), vec![3]);
}

fn load_err(rt: &Runtime, wat: &str) -> String {
    rt.load(wat.as_bytes()).expect_err("must be rejected").to_string()
}

#[tokio::test]
async fn bad_shapes_are_rejected_at_load() {
    let (rt, _) = runtime();
    let e = load_err(&rt, r#"(module (import "env" "memory" (memory 1 2)))"#);
    assert!(e.contains("unshared"), "{e}");
    let e = load_err(&rt, r#"(module (import "env" "abort" (func)))"#);
    assert!(e.contains("env.abort"), "{e}");
    let e = load_err(
        &rt,
        r#"(module (import "env" "memory" (memory 1 2 shared)) (import "env" "other" (func)))"#,
    );
    assert!(e.contains("env.other"), "{e}");
    let e = load_err(&rt, r#"(module (memory 1 2 shared))"#);
    assert!(matches!(
        rt.load(br#"(module (memory 1 2 shared))"#).unwrap_err(),
        LoadError::BadSharedMemory(_)
    ), "{e}");
    let spawn = r#"(import "orbit:thread/v1" "spawn" (func (param i64) (result i64)))"#;
    let e = load_err(&rt, &format!(r#"(module {spawn} (memory (export "memory") 1))"#));
    assert!(e.contains("shared memory import"), "{e}");
    let e = load_err(
        &rt,
        &format!(r#"(module {spawn} (import "env" "memory" (memory 1 2 shared)))"#),
    );
    assert!(e.contains("orin_thread_start"), "{e}");
    let e = load_err(
        &rt,
        &format!(
            r#"(module {spawn} (import "env" "memory" (memory 1 2 shared))
               (data (i32.const 0) "x")
               (func (export "orin_thread_start") (param i64 i64)))"#
        ),
    );
    assert!(e.contains("active data segment"), "{e}");
    let e = load_err(
        &rt,
        &format!(
            r#"(module {spawn} (import "env" "memory" (memory 1 2 shared))
               (func (export "orin_thread_start") (param i32 i64)))"#
        ),
    );
    assert!(e.contains("orin_thread_start"), "{e}");
    // 64-bit shared memory is not supported.
    let e = load_err(&rt, r#"(module (import "env" "memory" (memory i64 1 2 shared)))"#);
    assert!(e.contains("64-bit"), "{e}");
}
