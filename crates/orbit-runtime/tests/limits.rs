mod common;

use std::time::{Duration, Instant};

use common::*;
use orbit_runtime::{ExecError, Grants, InstantiateError, IsolateState, Limits};

const MEM: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "grow") (param i32) (result i32) (memory.grow (local.get 0)))
  (func (export "size") (result i32) (memory.size))
  (func (export "grow_or_trap") (param i32)
    (if (i32.eq (memory.grow (local.get 0)) (i32.const -1)) (then unreachable))))
"#;

#[tokio::test]
async fn fuel_exhaustion_is_out_of_fuel_and_measured() {
    let (rt, _) = runtime_metered();
    let art = load(&rt, SPIN);
    let mut iso = rt
        .spawn_isolate(
            &art,
            "t",
            &Grants::none(),
            Limits::default().with_fuel(100_000),
        )
        .await
        .unwrap();
    iso.init().await.unwrap();
    assert_eq!(iso.invoke("spin", &[]).await, Err(ExecError::OutOfFuel));
    assert_eq!(iso.state(), IsolateState::Failed);
    assert_eq!(iso.failure(), Some(&ExecError::OutOfFuel));
    let m = iso.metrics();
    let total = m.fuel_consumed.expect("metered engine measures fuel");
    assert!(total >= 100_000, "{total}");
    assert!(m.last_invocation.unwrap().fuel_consumed.unwrap() >= 100_000);
}

#[tokio::test]
async fn fuel_is_per_invocation() {
    let (rt, _) = runtime_metered();
    let art = load(&rt, SPIN);
    let mut iso = rt
        .spawn_isolate(&art, "t", &Grants::none(), Limits::default().with_fuel(100))
        .await
        .unwrap();
    iso.init().await.unwrap();
    for _ in 0..50 {
        assert_eq!(iso.invoke("add", &[1, 2]).await.unwrap(), vec![3]);
    }
    assert!(
        iso.metrics().fuel_consumed.unwrap() > 100,
        "total exceeds one invocation's budget"
    );
}

#[tokio::test]
async fn deadline_stops_runaway_without_fuel_and_runtime_stays_healthy() {
    // One slot only: the second isolate can run solely if the failed one released its slot.
    let (rt, _) = runtime_with(orbit_runtime::RuntimeConfig {
        max_concurrent_invocations: 1,
        ..orbit_runtime::RuntimeConfig::default()
    });
    let art = load(&rt, SPIN);
    let mut iso = rt
        .spawn_isolate(
            &art,
            "t",
            &Grants::none(),
            Limits::default().with_timeout(Duration::from_millis(100)),
        )
        .await
        .unwrap();
    iso.init().await.unwrap();
    let t = Instant::now();
    assert_eq!(
        iso.invoke("spin", &[]).await,
        Err(ExecError::DeadlineExceeded)
    );
    let elapsed = t.elapsed();
    assert!(elapsed >= Duration::from_millis(100), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    assert_eq!(iso.state(), IsolateState::Failed);
    assert!(iso.metrics().last_invocation.unwrap().wall_time >= Duration::from_millis(100));

    // The runtime is unaffected: a fresh isolate works and the run slot was released.
    let mut other = ready(&rt, SPIN).await;
    assert_eq!(other.invoke("add", &[40, 2]).await.unwrap(), vec![42]);
    assert_eq!(
        rt.available_invocation_slots(),
        rt.available_invocation_slots().max(1)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn runaway_guest_yields_to_other_tasks_on_the_same_thread() {
    let (rt, _) = runtime();
    let art = load(&rt, SPIN);
    let mut iso = rt
        .spawn_isolate(
            &art,
            "t",
            &Grants::none(),
            Limits::default().with_timeout(Duration::from_millis(300)),
        )
        .await
        .unwrap();
    iso.init().await.unwrap();

    // A task that is runnable whenever it is polled. It only gets polled if the guest hands
    // the single worker thread back to the executor.
    let polls = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let other = {
        let polls = std::sync::Arc::clone(&polls);
        tokio::spawn(async move {
            loop {
                polls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tokio::task::yield_now().await;
            }
        })
    };
    let result = iso.invoke("spin", &[]).await;
    other.abort();
    assert_eq!(result, Err(ExecError::DeadlineExceeded));
    // 300 ms at a 10 ms tick is about 30 yields; allow for a loaded machine.
    let n = polls.load(std::sync::atomic::Ordering::Relaxed);
    assert!(n >= 5, "other task was polled only {n} times");
}

#[tokio::test]
async fn memory_grow_beyond_limit_returns_minus_one_and_is_counted() {
    let (rt, _) = runtime();
    let art = load(&rt, MEM);
    // 2 pages allowed.
    let limits = Limits::default().with_max_memory(2 * 65536);
    let mut iso = rt
        .spawn_isolate(&art, "t", &Grants::none(), limits)
        .await
        .unwrap();
    iso.init().await.unwrap();
    assert_eq!(iso.metrics().memory_bytes, 65536);
    assert_eq!(iso.invoke("grow", &[1]).await.unwrap(), vec![1]);
    assert_eq!(iso.invoke("grow", &[1]).await.unwrap(), vec![-1]);
    assert_eq!(iso.invoke("grow", &[1000]).await.unwrap(), vec![-1]);
    assert_eq!(iso.invoke("size", &[]).await.unwrap(), vec![2]);
    // Growth failure alone does not fail the isolate.
    assert_eq!(iso.state(), IsolateState::Idle);
    let m = iso.metrics();
    assert_eq!(m.memory_grow_denied, 2);
    assert_eq!(m.memory_bytes, 2 * 65536);
    assert_eq!(m.peak_memory_bytes, 2 * 65536);
}

#[tokio::test]
async fn guest_that_traps_on_failed_grow_ends_failed_with_memory_limit() {
    let (rt, _) = runtime();
    let art = load(&rt, MEM);
    let limits = Limits::default().with_max_memory(65536);
    let mut iso = rt
        .spawn_isolate(&art, "t", &Grants::none(), limits)
        .await
        .unwrap();
    iso.init().await.unwrap();
    let e = iso.invoke("grow_or_trap", &[1]).await.unwrap_err();
    assert!(matches!(e, ExecError::MemoryLimit { denied: 1, .. }), "{e}");
    assert_eq!(iso.state(), IsolateState::Failed);
    assert_eq!(iso.metrics().memory_grow_denied, 1);
}

#[tokio::test]
async fn initial_memory_over_limit_fails_instantiation() {
    let (rt, _) = runtime();
    let art = load(&rt, r#"(module (memory 100))"#); // 6.4 MB
    let limits = Limits::default().with_max_memory(1024 * 1024);
    let e = rt
        .spawn_isolate(&art, "t", &Grants::none(), limits)
        .await
        .unwrap_err();
    assert!(matches!(e, InstantiateError::ResourceLimit(_)), "{e}");
    assert_eq!(rt.reserved_memory_bytes(), 0);
}

#[tokio::test]
async fn table_growth_is_limited() {
    let (rt, _) = runtime();
    let art = load(
        &rt,
        r#"(module (table 1 funcref)
             (func (export "grow") (param i32) (result i32) (table.grow (ref.null func) (local.get 0))))"#,
    );
    let mut iso = rt
        .spawn_isolate(
            &art,
            "t",
            &Grants::none(),
            Limits {
                max_table_elements: 10,
                ..Limits::default()
            },
        )
        .await
        .unwrap();
    iso.init().await.unwrap();
    assert_eq!(iso.invoke("grow", &[5]).await.unwrap(), vec![1]);
    assert_eq!(iso.invoke("grow", &[50]).await.unwrap(), vec![-1]);
    assert_eq!(iso.metrics().table_grow_denied, 1);
}

#[tokio::test]
async fn pooling_allocator_enforces_the_same_limits() {
    use orbit_runtime::{EngineConfig, PoolingConfig, RuntimeConfig};
    let (rt, _) = runtime_metered_with(RuntimeConfig {
        engine: EngineConfig {
            pooling: Some(PoolingConfig {
                max_instances: 8,
                max_memory_bytes: 4 * 1024 * 1024,
                ..PoolingConfig::default()
            }),
            ..EngineConfig::default()
        },
        ..RuntimeConfig::default()
    });
    let art = load(&rt, MEM);
    let limits = Limits::default()
        .with_max_memory(2 * 65536)
        .with_fuel(10_000);
    let mut iso = rt
        .spawn_isolate(&art, "t", &Grants::none(), limits)
        .await
        .unwrap();
    iso.init().await.unwrap();
    assert_eq!(iso.invoke("grow", &[1]).await.unwrap(), vec![1]);
    assert_eq!(iso.invoke("grow", &[1]).await.unwrap(), vec![-1]);
    iso.terminate();
    // Slots are reusable after teardown.
    for _ in 0..20 {
        let mut i = rt
            .spawn_isolate(&art, "t", &Grants::none(), Limits::default())
            .await
            .unwrap();
        i.init().await.unwrap();
        i.terminate();
    }
}

#[tokio::test]
async fn instantiation_fuel_is_bounded_by_data_initialisation() {
    let (rt, _) = runtime_metered();
    let variants = [
        (
            "plain",
            r#"(module (memory (export "memory") 1) (func (export "f")))"#,
        ),
        (
            "data",
            r#"(module (memory (export "memory") 1) (data (i32.const 16) "hello from guest") (func (export "f")))"#,
        ),
        (
            "custom",
            r#"(module (@custom "orbit.manifest" "{\"abi\":1,\"requires\":[]}") (memory (export "memory") 1) (func (export "f")))"#,
        ),
    ];
    for (name, wat) in variants {
        let art = load(&rt, wat);
        let iso = rt
            .spawn_isolate(
                &art,
                "t",
                &Grants::none(),
                Limits::default().with_fuel(1_000_000),
            )
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        // With a copy-on-write memory image (Linux) instantiation costs no fuel. Without one
        // (observed on Windows and macOS), Wasmtime charges the data initialisation, about one
        // unit per byte of the host pages it touches: 4097 with 4 KiB pages. A start-less
        // module must never cost more than its initialised memory.
        let used = iso.metrics().fuel_consumed.unwrap();
        assert!(used <= 65_537, "{name}: instantiation consumed {used} fuel");
    }
}
