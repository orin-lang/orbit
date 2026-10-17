mod common;

use common::*;
use orbit_runtime::{IsolateState, Limits};

#[tokio::test]
async fn instances_of_one_artifact_do_not_share_state() {
    let (rt, _) = runtime();
    let art = load(&rt, COUNTER);
    let none = orbit_runtime::Grants::none();
    let mut a = rt
        .spawn_isolate(&art, "t", &none, Limits::default())
        .await
        .unwrap();
    let mut b = rt
        .spawn_isolate(&art, "t", &none, Limits::default())
        .await
        .unwrap();
    a.init().await.unwrap();
    b.init().await.unwrap();

    for _ in 0..3 {
        a.invoke("bump", &[]).await.unwrap();
    }
    assert_eq!(a.invoke("global", &[]).await.unwrap(), vec![3]);
    assert_eq!(a.invoke("mem", &[]).await.unwrap(), vec![30]);
    // B never saw any of it.
    assert_eq!(b.invoke("global", &[]).await.unwrap(), vec![0]);
    assert_eq!(b.invoke("mem", &[]).await.unwrap(), vec![0]);
    assert_eq!(b.invoke("bump", &[]).await.unwrap(), vec![1]);
    assert_eq!(a.invoke("global", &[]).await.unwrap(), vec![3]);
    assert_ne!(a.id(), b.id());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn different_modules_run_concurrently() {
    let (rt, _) = runtime_metered();
    let counter = load(&rt, COUNTER);
    let spin = load(&rt, SPIN);
    let none = orbit_runtime::Grants::none();
    let mut c = rt
        .spawn_isolate(&counter, "a", &none, Limits::default())
        .await
        .unwrap();
    let mut s = rt
        .spawn_isolate(&spin, "b", &none, Limits::default().with_fuel(5_000_000))
        .await
        .unwrap();
    c.init().await.unwrap();
    s.init().await.unwrap();

    let t1 = tokio::spawn(async move {
        for i in 1..=1000 {
            assert_eq!(c.invoke("bump", &[]).await.unwrap(), vec![i]);
        }
        c
    });
    let t2 = tokio::spawn(async move {
        let err = s.invoke("spin", &[]).await.unwrap_err();
        (s, err)
    });
    let c = t1.await.unwrap();
    let (s, err) = t2.await.unwrap();
    assert_eq!(c.state(), IsolateState::Idle);
    assert_eq!(err, orbit_runtime::ExecError::OutOfFuel);
    assert_eq!(s.state(), IsolateState::Failed);
}

#[tokio::test]
async fn invoke_arguments_and_results() {
    let (rt, _) = runtime();
    let mut iso = ready(&rt, SPIN).await;
    assert_eq!(iso.invoke("add", &[2, 3]).await.unwrap(), vec![5]);
    assert_eq!(iso.invoke("add", &[-1, 0]).await.unwrap(), vec![-1]);
    assert_eq!(
        iso.invoke("add", &[0xFFFF_FFFF, 0]).await.unwrap(),
        vec![-1]
    );
    use orbit_runtime::ExecError::*;
    assert!(matches!(
        iso.invoke("add", &[1]).await,
        Err(BadArguments(_))
    ));
    assert!(matches!(
        iso.invoke("add", &[1 << 40, 0]).await,
        Err(BadArguments(_))
    ));
    assert!(matches!(
        iso.invoke("nope", &[]).await,
        Err(NoSuchExport(_))
    ));
    // Argument errors do not poison the isolate.
    assert_eq!(iso.state(), IsolateState::Idle);
    let m = iso.metrics();
    // The default engine has no fuel metering: fuel is not measured (not "0").
    assert_eq!(m.fuel_consumed, None);
    assert_eq!(m.last_invocation.unwrap().fuel_consumed, None);
    assert_eq!(m.invocations, 3);
}
