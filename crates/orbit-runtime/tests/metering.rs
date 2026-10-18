//! Fuel metering is opt-in (ADR-0008); `Trap`-only post-mortem reads.

mod common;

use std::time::{Duration, Instant};

use common::*;
use orbit_runtime::{ExecError, Grants, InstantiateError, IsolateState, Limits};

#[tokio::test]
async fn fuel_without_metering_is_refused_not_ignored() {
    let (rt, _) = runtime();
    assert!(!rt.engine().fuel_metering());
    let art = load(&rt, SPIN);
    let err = rt
        .spawn_isolate(
            &art,
            "t",
            &Grants::none(),
            Limits::default().with_fuel(1000),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, InstantiateError::FuelMeteringDisabled),
        "{err}"
    );
    // Nothing was reserved by the refused spawn.
    assert_eq!(rt.reserved_memory_bytes(), 0);
}

#[tokio::test]
async fn metered_runtime_accepts_fuel_and_unlimited_budgets() {
    let (rt, _) = runtime_metered();
    assert!(rt.engine().fuel_metering());
    let art = load(&rt, SPIN);
    let mut iso = rt
        .spawn_isolate(&art, "t", &Grants::none(), Limits::default())
        .await
        .unwrap();
    iso.init().await.unwrap();
    iso.invoke("add", &[1, 2]).await.unwrap();
    assert!(iso.metrics().fuel_consumed.unwrap() > 0);
}

#[tokio::test]
async fn unmetered_runtime_with_timeout_stops_a_runaway_loop() {
    let (rt, _) = runtime();
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
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    assert_eq!(iso.state(), IsolateState::Failed);
}

#[tokio::test]
async fn unmetered_metrics_say_fuel_is_not_measured() {
    let (rt, _) = runtime();
    let mut iso = ready(&rt, SPIN).await;
    iso.invoke("add", &[1, 2]).await.unwrap();
    let m = iso.metrics();
    assert_eq!(m.fuel_consumed, None);
    assert_eq!(m.last_invocation.unwrap().fuel_consumed, None);
    assert!(m.last_invocation.unwrap().wall_time > Duration::ZERO);
    iso.terminate();
    assert_eq!(iso.metrics().fuel_consumed, None);
}

#[tokio::test]
async fn metered_pooling_and_unmetered_pooling_both_work() {
    use orbit_runtime::{EngineConfig, PoolingConfig, RuntimeConfig};
    for metering in [false, true] {
        let (rt, _) = runtime_with(RuntimeConfig {
            engine: EngineConfig {
                pooling: Some(PoolingConfig {
                    max_instances: 4,
                    max_memory_bytes: 4 * 1024 * 1024,
                    ..PoolingConfig::default()
                }),
                fuel_metering: metering,
                ..EngineConfig::default()
            },
            ..RuntimeConfig::default()
        });
        let mut iso = ready(&rt, SPIN).await;
        assert_eq!(iso.invoke("add", &[40, 2]).await.unwrap(), vec![42]);
        assert_eq!(iso.metrics().fuel_consumed.is_some(), metering);
    }
}

// --- post-mortem ---------------------------------------------------------------------------

const POST: &str = r#"
(module
  (memory (export "memory") 1)
  (global $status (mut i32) (i32.const 0))
  (func (export "set_status_then_trap") (param i32)
    (global.set $status (local.get 0))
    unreachable)
  (func (export "ok") (result i32) (i32.const 1))
  (func (export "spin") (loop $l (br $l)))
  (func (export "grow_or_trap") (param i32)
    (if (i32.eq (memory.grow (local.get 0)) (i32.const -1)) (then unreachable)))
  (func (export "status") (result i32) (global.get $status))
  (func (export "status_spins") (result i32) (loop $l (br $l)) (i32.const 3))
  (func (export "status_traps") (result i32) unreachable)
  (func (export "takes_arg") (param i32) (result i32) (local.get 0))
  (func (export "returns_i64") (result i64) (i64.const 1))
  (func (export "returns_nothing")))
"#;

async fn trapped(rt: &orbit_runtime::Runtime, status: i64) -> orbit_runtime::Isolate {
    let mut iso = ready(rt, POST).await;
    let err = iso
        .invoke("set_status_then_trap", &[status])
        .await
        .unwrap_err();
    assert!(matches!(err, ExecError::Trap(_)), "{err}");
    assert_eq!(iso.state(), IsolateState::Failed);
    iso
}

#[tokio::test]
async fn post_mortem_reads_status_after_a_trap_and_stays_failed() {
    let (rt, _) = runtime();
    let mut iso = trapped(&rt, 5).await;
    let before = iso.failure().cloned();
    assert_eq!(iso.post_mortem_i32("status").await, Some(5));
    // Repeatable, never changes the state or the recorded failure, not an invocation.
    assert_eq!(iso.post_mortem_i32("status").await, Some(5));
    assert_eq!(iso.state(), IsolateState::Failed);
    assert_eq!(iso.failure().cloned(), before);
    assert_eq!(iso.metrics().invocations, 1); // only the trapping one
    // Normal invocations stay refused.
    assert!(matches!(
        iso.invoke("ok", &[]).await,
        Err(ExecError::InvalidState(IsolateState::Failed))
    ));
    iso.terminate();
}

#[tokio::test]
async fn post_mortem_works_on_a_metered_engine_and_is_charged() {
    let (rt, _) = runtime_metered();
    let mut iso = rt
        .spawn_isolate(
            &load(&rt, POST),
            "t",
            &Grants::none(),
            Limits::default().with_fuel(1_000_000),
        )
        .await
        .unwrap();
    iso.init().await.unwrap();
    iso.invoke("set_status_then_trap", &[-1]).await.unwrap_err();
    let before = iso.metrics().fuel_consumed.unwrap();
    assert_eq!(iso.post_mortem_i32("status").await, Some(-1));
    assert!(iso.metrics().fuel_consumed.unwrap() > before);
}

#[tokio::test]
async fn post_mortem_refused_outside_failed_after_trap() {
    let (rt, _) = runtime();
    // Not failed.
    let mut iso = ready(&rt, POST).await;
    assert_eq!(iso.post_mortem_i32("status").await, None);
    iso.invoke("ok", &[]).await.unwrap();
    assert_eq!(iso.post_mortem_i32("status").await, None);
    iso.terminate();
    assert_eq!(iso.post_mortem_i32("status").await, None);

    // Failed by a deadline: not allowed.
    let art = load(&rt, POST);
    let mut iso = rt
        .spawn_isolate(
            &art,
            "t",
            &Grants::none(),
            Limits::default().with_timeout(Duration::from_millis(50)),
        )
        .await
        .unwrap();
    iso.init().await.unwrap();
    assert_eq!(
        iso.invoke("spin", &[]).await,
        Err(ExecError::DeadlineExceeded)
    );
    assert_eq!(iso.post_mortem_i32("status").await, None);

    // Failed by fuel: not allowed.
    let (mrt, _) = runtime_metered();
    let mut iso = mrt
        .spawn_isolate(
            &load(&mrt, POST),
            "t",
            &Grants::none(),
            Limits::default().with_fuel(10_000),
        )
        .await
        .unwrap();
    iso.init().await.unwrap();
    assert_eq!(iso.invoke("spin", &[]).await, Err(ExecError::OutOfFuel));
    assert_eq!(iso.post_mortem_i32("status").await, None);

    // Failed by a memory-limit trap: not allowed (MemoryLimit, not Trap).
    let mut iso = rt
        .spawn_isolate(
            &art,
            "t",
            &Grants::none(),
            Limits::default().with_max_memory(65536),
        )
        .await
        .unwrap();
    iso.init().await.unwrap();
    let err = iso.invoke("grow_or_trap", &[1]).await.unwrap_err();
    assert!(matches!(err, ExecError::MemoryLimit { .. }), "{err}");
    assert_eq!(iso.post_mortem_i32("status").await, None);
}

#[tokio::test]
async fn post_mortem_requires_an_i32_nullary_export() {
    let (rt, _) = runtime();
    let mut iso = trapped(&rt, 9).await;
    for name in [
        "nope",
        "takes_arg",
        "returns_i64",
        "returns_nothing",
        "set_status_then_trap",
        "memory",
    ] {
        assert_eq!(iso.post_mortem_i32(name).await, None, "{name}");
    }
    assert_eq!(iso.post_mortem_i32("status").await, Some(9));
}

#[tokio::test]
async fn post_mortem_that_traps_or_runs_away_is_none_and_keeps_the_original_failure() {
    let (rt, _) = runtime();
    let mut iso = trapped(&rt, 2).await;
    let original = iso.failure().cloned();
    assert_eq!(iso.post_mortem_i32("status_traps").await, None);
    let t = Instant::now();
    assert_eq!(iso.post_mortem_i32("status_spins").await, None);
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    assert_eq!(iso.state(), IsolateState::Failed);
    assert_eq!(iso.failure().cloned(), original);
    // Still usable afterwards.
    assert_eq!(iso.post_mortem_i32("status").await, Some(2));
}

#[tokio::test]
async fn post_mortem_runaway_is_bounded_on_a_metered_engine_too() {
    let (rt, _) = runtime_metered();
    let mut iso = rt
        .spawn_isolate(&load(&rt, POST), "t", &Grants::none(), Limits::default())
        .await
        .unwrap();
    iso.init().await.unwrap();
    iso.invoke("set_status_then_trap", &[1]).await.unwrap_err();
    assert_eq!(iso.post_mortem_i32("status_spins").await, None);
    assert_eq!(iso.post_mortem_i32("status").await, Some(1));
}
