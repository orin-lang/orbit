mod common;

use common::*;
use orbit_runtime::{AdmissionError, ExecError, Grants, InstantiateError, IsolateState, Limits};

const INIT_OK: &str = r#"
(module
  (global $g (mut i32) (i32.const 0))
  (func (export "_initialize") (global.set $g (i32.const 5)))
  (func (export "orbit_init") (result i32)
    (if (result i32) (i32.eq (global.get $g) (i32.const 5)) (then (i32.const 0)) (else (i32.const 99))))
  (func (export "global") (result i32) (global.get $g)))
"#;

#[tokio::test]
async fn init_runs_initialize_then_orbit_init() {
    let (rt, _) = runtime();
    let art = load(&rt, INIT_OK);
    let mut iso = rt
        .spawn_isolate(&art, "t", &Grants::none(), Limits::default())
        .await
        .unwrap();
    assert_eq!(iso.state(), IsolateState::Created);
    // No invocation before init.
    assert_eq!(
        iso.invoke("global", &[]).await,
        Err(ExecError::InvalidState(IsolateState::Created))
    );
    iso.init().await.unwrap();
    assert_eq!(iso.state(), IsolateState::Initialized);
    assert_eq!(iso.invoke("global", &[]).await.unwrap(), vec![5]);
    assert_eq!(iso.state(), IsolateState::Idle);
    // Init is one-shot.
    assert_eq!(
        iso.init().await,
        Err(ExecError::InvalidState(IsolateState::Idle))
    );
}

#[tokio::test]
async fn module_without_init_exports_initialises() {
    let (rt, _) = runtime();
    let mut iso = ready(&rt, SPIN).await;
    assert_eq!(iso.state(), IsolateState::Initialized);
    iso.terminate();
}

#[tokio::test]
async fn orbit_init_nonzero_fails_the_isolate() {
    let (rt, _) = runtime();
    let art = load(
        &rt,
        r#"(module (func (export "orbit_init") (result i32) (i32.const 1)) (func (export "f")))"#,
    );
    let mut iso = rt
        .spawn_isolate(&art, "t", &Grants::none(), Limits::default())
        .await
        .unwrap();
    assert_eq!(iso.init().await, Err(ExecError::InitFailed(1)));
    assert_eq!(iso.state(), IsolateState::Failed);
    assert_eq!(iso.failure(), Some(&ExecError::InitFailed(1)));
    assert_eq!(
        iso.invoke("f", &[]).await,
        Err(ExecError::InvalidState(IsolateState::Failed))
    );
}

#[tokio::test]
async fn trapping_init_fails_the_isolate() {
    let (rt, _) = runtime();
    for wat in [
        r#"(module (func (export "orbit_init") unreachable))"#,
        r#"(module (func (export "_initialize") unreachable))"#,
    ] {
        let art = load(&rt, wat);
        let mut iso = rt
            .spawn_isolate(&art, "t", &Grants::none(), Limits::default())
            .await
            .unwrap();
        let e = iso.init().await.unwrap_err();
        assert!(
            matches!(&e, ExecError::Trap(m) if m.contains("unreachable")),
            "{e}"
        );
        assert_eq!(iso.state(), IsolateState::Failed);
    }
}

#[tokio::test]
async fn start_function_trap_fails_instantiation() {
    let (rt, _) = runtime();
    let art = load(&rt, r#"(module (func $s unreachable) (start $s))"#);
    let e = rt
        .spawn_isolate(&art, "t", &Grants::none(), Limits::default())
        .await
        .unwrap_err();
    assert!(matches!(e, InstantiateError::Instantiate(_)), "{e}");
    assert_eq!(rt.reserved_memory_bytes(), 0);
}

#[tokio::test]
async fn trap_fails_isolate_and_terminate_releases_everything() {
    let (rt, _) = runtime();
    let art = load(
        &rt,
        r#"(module (memory 1) (func (export "boom") unreachable) (func (export "ok")))"#,
    );
    let limits = Limits::default().with_max_memory(1024 * 1024);
    let mut iso = rt
        .spawn_isolate(&art, "t", &Grants::none(), limits.clone())
        .await
        .unwrap();
    iso.init().await.unwrap();
    assert_eq!(rt.reserved_memory_bytes(), 1024 * 1024);

    let e = iso.invoke("boom", &[]).await.unwrap_err();
    assert!(matches!(e, ExecError::Trap(_)), "{e}");
    assert_eq!(iso.state(), IsolateState::Failed);
    assert_eq!(
        iso.invoke("ok", &[]).await,
        Err(ExecError::InvalidState(IsolateState::Failed))
    );
    assert_eq!(
        rt.reserved_memory_bytes(),
        1024 * 1024,
        "a failed isolate still holds its budget"
    );

    iso.terminate();
    assert_eq!(iso.state(), IsolateState::Terminated);
    assert_eq!(rt.reserved_memory_bytes(), 0);
    assert_eq!(
        iso.invoke("ok", &[]).await,
        Err(ExecError::InvalidState(IsolateState::Terminated))
    );
    assert_eq!(iso.metrics().state, IsolateState::Terminated);
    iso.terminate(); // idempotent
    assert_eq!(rt.reserved_memory_bytes(), 0);
}

#[tokio::test]
async fn memory_admission_budget_is_enforced_and_released() {
    let (rt, _) = runtime_with(orbit_runtime::RuntimeConfig {
        memory_budget_bytes: 3 * 1024 * 1024,
        ..orbit_runtime::RuntimeConfig::default()
    });
    let art = load(&rt, COUNTER);
    let limits = Limits::default().with_max_memory(2 * 1024 * 1024);
    let none = Grants::none();
    let mut a = rt
        .spawn_isolate(&art, "t", &none, limits.clone())
        .await
        .unwrap();
    let e = rt
        .spawn_isolate(&art, "t", &none, limits.clone())
        .await
        .unwrap_err();
    assert!(
        matches!(e, InstantiateError::Admission(AdmissionError::MemoryBudget { requested, available })
            if requested == 2 * 1024 * 1024 && available == 1024 * 1024),
        "{e}"
    );
    a.terminate();
    let b = rt
        .spawn_isolate(&art, "t", &none, limits.clone())
        .await
        .unwrap();
    drop(b); // dropping also releases
    assert_eq!(rt.reserved_memory_bytes(), 0);
    rt.spawn_isolate(&art, "t", &none, limits).await.unwrap();
}
