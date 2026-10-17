use orbit_abi::CapabilityId;
use orbit_caps::{ClockCapability, ManualClock};
use orbit_runtime::{
    CapabilityRegistry, Grants, InstantiateError, Isolate, Limits, Runtime, RuntimeConfig,
};
use serde_json::json;

const CLOCK: &str = r#"
(module
  (import "orbit:clock/v1" "now_unix_nanos" (func $now (result i64)))
  (import "orbit:clock/v1" "monotonic_nanos" (func $mono (result i64)))
  (func (export "now") (result i64) (call $now))
  (func (export "mono") (result i64) (call $mono)))
"#;

fn runtime(cap: ClockCapability) -> Runtime {
    Runtime::new(
        RuntimeConfig::default(),
        CapabilityRegistry::new().with(cap),
    )
    .unwrap()
}

async fn spawn(rt: &Runtime, grants: &Grants) -> Isolate {
    let art = rt.load(CLOCK.as_bytes()).unwrap();
    let mut iso = rt
        .spawn_isolate(&art, "t", grants, Limits::default())
        .await
        .unwrap();
    iso.init().await.unwrap();
    iso
}

fn grants(config: serde_json::Value) -> Grants {
    Grants::none().grant_with(CapabilityId::clock_v1(), config)
}

async fn call(iso: &mut Isolate, f: &str) -> i64 {
    iso.invoke(f, &[]).await.unwrap()[0]
}

#[tokio::test]
async fn default_resolution_is_one_millisecond() {
    let clock = ManualClock::new(1_700_000_000_123_456_789);
    let rt = runtime(ClockCapability::new(clock.clone()));
    let mut iso = spawn(&rt, &Grants::none().grant(CapabilityId::clock_v1())).await;
    assert_eq!(call(&mut iso, "now").await, 1_700_000_000_123_000_000);
    clock.advance(2_999_999);
    assert_eq!(call(&mut iso, "now").await, 1_700_000_000_126_000_000);
    // Monotonic time starts at the instance's creation.
    assert_eq!(call(&mut iso, "mono").await, 2_000_000);
}

#[tokio::test]
async fn resolution_is_configurable_and_zero_means_exact() {
    let clock = ManualClock::new(10_000_000_123);
    let rt = runtime(ClockCapability::new(clock.clone()));
    let mut coarse = spawn(&rt, &grants(json!({"resolution_ns": 1_000_000_000}))).await;
    let mut exact = spawn(&rt, &grants(json!({"resolution_ns": 0}))).await;
    clock.advance(1_500_000_007);
    assert_eq!(call(&mut coarse, "now").await, 11_000_000_000);
    assert_eq!(call(&mut coarse, "mono").await, 1_000_000_000);
    assert_eq!(call(&mut exact, "now").await, 11_500_000_130);
    assert_eq!(call(&mut exact, "mono").await, 1_500_000_007);
}

#[tokio::test]
async fn monotonic_is_since_instance_creation_and_never_decreases() {
    let clock = ManualClock::new(0);
    let rt = runtime(ClockCapability::new(clock.clone()));
    clock.advance(5_000_000_000);
    let mut early = spawn(&rt, &grants(json!({"resolution_ns": 1}))).await;
    clock.advance(7);
    let mut late = spawn(&rt, &grants(json!({"resolution_ns": 1}))).await;
    clock.advance(3);
    assert_eq!(call(&mut early, "mono").await, 10);
    assert_eq!(call(&mut late, "mono").await, 3);
    // The wall clock may go backwards; the monotonic one must not.
    clock.set_unix_nanos(1);
    assert_eq!(call(&mut early, "mono").await, 10);
}

#[tokio::test]
async fn system_clock_is_non_decreasing_and_coarsened() {
    let rt = runtime(ClockCapability::system());
    let mut iso = spawn(&rt, &grants(json!({"resolution_ns": 1_000_000}))).await;
    let mut last_mono = 0;
    let mut last_now = 0;
    for _ in 0..200 {
        let m = call(&mut iso, "mono").await;
        let n = call(&mut iso, "now").await;
        assert!(m >= last_mono);
        assert_eq!(m % 1_000_000, 0);
        assert_eq!(n % 1_000_000, 0);
        last_mono = m;
        last_now = n;
    }
    // After 2020-01-01.
    assert!(last_now > 1_577_836_800_000_000_000);
    let m = iso.metrics();
    assert_eq!(m.host_calls[&CapabilityId::clock_v1()], 400);
}

#[tokio::test]
async fn clock_requires_a_grant() {
    let rt = runtime(ClockCapability::system());
    let art = rt.load(CLOCK.as_bytes()).unwrap();
    let e = rt
        .spawn_isolate(&art, "t", &Grants::none(), Limits::default())
        .await
        .unwrap_err();
    assert!(matches!(e, InstantiateError::NotGranted(_)), "{e}");
}
