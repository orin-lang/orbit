use orbit_abi::{CapabilityId, status};
use orbit_caps::{LogCapability, LogLevel, MemorySink};
use orbit_runtime::{
    CapabilityRegistry, Grants, InstantiateError, Isolate, IsolateState, Limits, Runtime,
    RuntimeConfig,
};

fn setup() -> (Runtime, MemorySink) {
    let sink = MemorySink::new();
    let registry = CapabilityRegistry::new().with(LogCapability::new(sink.clone()));
    (
        Runtime::new(RuntimeConfig::default(), registry).unwrap(),
        sink,
    )
}

fn grant() -> Grants {
    Grants::none().grant(CapabilityId::log_v1())
}

/// 32-bit memory guest. `hello` logs "hello"; `log(level, ptr, len)` is a raw pass-through;
/// `poke(addr, byte)` writes a byte so tests can build invalid UTF-8.
const LOG32: &str = r#"
(module
  (import "orbit:log/v1" "write" (func $w (param i32 i64 i64) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 16) "hello")
  (func (export "hello") (result i32) (call $w (i32.const 2) (i64.const 16) (i64.const 5)))
  (func (export "log") (param i32 i64 i64) (result i32)
    (call $w (local.get 0) (local.get 1) (local.get 2)))
  (func (export "poke") (param i32 i32) (i32.store8 (local.get 0) (local.get 1)))
  (func (export "mem_size") (result i32) (memory.size)))
"#;

/// The same with a 64-bit memory.
const LOG64: &str = r#"
(module
  (import "orbit:log/v1" "write" (func $w (param i32 i64 i64) (result i32)))
  (memory (export "memory") i64 1)
  (data (i64.const 16) "hello")
  (func (export "hello") (result i32) (call $w (i32.const 2) (i64.const 16) (i64.const 5)))
  (func (export "log") (param i32 i64 i64) (result i32)
    (call $w (local.get 0) (local.get 1) (local.get 2)))
  (func (export "poke") (param i64 i32) (i32.store8 (local.get 0) (local.get 1))))
"#;

async fn spawn(rt: &Runtime, wat: &str, tenant: &str, limits: Limits) -> Isolate {
    let art = rt.load(wat.as_bytes()).unwrap();
    let mut iso = rt
        .spawn_isolate(&art, tenant, &grant(), limits)
        .await
        .unwrap();
    iso.init().await.unwrap();
    iso
}

#[tokio::test]
async fn log_without_grant_is_not_granted_and_nothing_runs() {
    let (rt, sink) = setup();
    // The module logs from its start function; if it ever ran we would see a record.
    let art = rt
        .load(
            br#"(module
              (import "orbit:log/v1" "write" (func $w (param i32 i64 i64) (result i32)))
              (memory (export "memory") 1)
              (data (i32.const 0) "pwned")
              (func $s (drop (call $w (i32.const 4) (i64.const 0) (i64.const 5))))
              (start $s))"#,
        )
        .unwrap();
    assert_eq!(art.required().len(), 1);
    let e = rt
        .spawn_isolate(&art, "t", &Grants::none(), Limits::default())
        .await
        .unwrap_err();
    assert!(
        matches!(e, InstantiateError::NotGranted(ref id) if *id == CapabilityId::log_v1()),
        "{e}"
    );
    assert_eq!(e.to_string(), "capability not granted: log/v1");
    // Granting an unrelated capability does not help.
    let other = Grants::none().grant(CapabilityId::kv_v1());
    assert!(matches!(
        rt.spawn_isolate(&art, "t", &other, Limits::default())
            .await
            .unwrap_err(),
        InstantiateError::NotGranted(_)
    ));
    assert!(sink.records().is_empty());
    assert_eq!(rt.reserved_memory_bytes(), 0);
}

#[tokio::test]
async fn granted_logs_reach_the_sink_with_host_attached_identity() {
    let (rt, sink) = setup();
    let mut a = spawn(&rt, LOG32, "acme", Limits::default()).await;
    let mut b = spawn(&rt, LOG32, "globex", Limits::default()).await;
    assert_eq!(a.invoke("hello", &[]).await.unwrap(), vec![0]);
    assert_eq!(b.invoke("hello", &[]).await.unwrap(), vec![0]);
    assert_eq!(a.invoke("log", &[4, 16, 5]).await.unwrap(), vec![0]);

    let r = sink.records();
    assert_eq!(r.len(), 3);
    assert_eq!(
        (r[0].instance, r[0].tenant.as_str(), r[0].level),
        (a.id(), "acme", LogLevel::Info)
    );
    assert_eq!((r[1].instance, r[1].tenant.as_str()), (b.id(), "globex"));
    assert_eq!(r[2].level, LogLevel::Error);
    assert!(r.iter().all(|r| r.message == "hello"));
    assert_eq!(a.metrics().bytes_logged, 10);
    assert_eq!(a.metrics().host_calls[&CapabilityId::log_v1()], 2);
    assert_eq!(b.metrics().host_calls[&CapabilityId::log_v1()], 1);
}

async fn status_of(iso: &mut Isolate, level: i64, ptr: i64, len: i64) -> i32 {
    let out = iso.invoke("log", &[level, ptr, len]).await.unwrap();
    i32::try_from(out[0]).unwrap()
}

async fn validation(wat: &str) {
    let (rt, sink) = setup();
    let mut iso = spawn(&rt, wat, "t", Limits::default()).await;
    let page = 65536;

    // Valid boundaries.
    assert_eq!(status_of(&mut iso, 2, 16, 5).await, status::OK);
    assert_eq!(
        status_of(&mut iso, 2, page, 0).await,
        status::OK,
        "empty message at the end"
    );
    assert_eq!(status_of(&mut iso, 2, page - 5, 5).await, status::OK);
    let before = sink.records().len();

    // Out of bounds, negative, overflowing.
    for (ptr, len) in [
        (page - 4, 5),
        (page, 1),
        (-1, 1),
        (-16, 0),
        (0, -1),
        (-1, -1),
        (i64::MAX, 1),
        (i64::MIN, 1),
        (-8, 16), // ptr is 2^64-8 as unsigned: ptr + len wraps
        (i64::MAX - 2, 8),
    ] {
        assert_eq!(
            status_of(&mut iso, 2, ptr, len).await,
            status::ERR_INVALID,
            "ptr={ptr} len={len}"
        );
    }
    // Over the 16 KiB message cap, even though it is in bounds.
    assert_eq!(
        status_of(&mut iso, 2, 0, 16 * 1024 + 1).await,
        status::ERR_TOO_LARGE
    );
    assert_eq!(
        status_of(&mut iso, 2, 0, i64::MAX).await,
        status::ERR_TOO_LARGE
    );
    assert_eq!(status_of(&mut iso, 2, 0, 16 * 1024).await, status::OK);
    // Bad levels.
    for level in [-1, 5, 100, i64::from(i32::MIN)] {
        assert_eq!(
            status_of(&mut iso, level, 16, 5).await,
            status::ERR_INVALID,
            "level {level}"
        );
    }
    // Nothing was logged by the rejected calls.
    assert_eq!(sink.records().len(), before + 1);

    // Invalid UTF-8: overwrite one byte of "hello" with 0xFF.
    iso.invoke("poke", &[16, 0xFF]).await.unwrap();
    assert_eq!(status_of(&mut iso, 2, 16, 5).await, status::ERR_INVALID);
    // A truncated multi-byte sequence at the end.
    iso.invoke("poke", &[16, 0xE2]).await.unwrap();
    assert_eq!(status_of(&mut iso, 2, 16, 1).await, status::ERR_INVALID);
    // Valid multi-byte text is fine.
    iso.invoke("poke", &[16, 0xC3]).await.unwrap();
    iso.invoke("poke", &[17, 0xA9]).await.unwrap();
    assert_eq!(status_of(&mut iso, 2, 16, 2).await, status::OK);
    assert_eq!(sink.records().last().unwrap().message, "\u{e9}");

    // The host is fine and so is the isolate: bad arguments are statuses, not traps.
    assert_eq!(iso.state(), IsolateState::Idle);
}

#[tokio::test]
async fn host_validates_log_arguments_memory32() {
    validation(LOG32).await;
}

#[tokio::test]
async fn host_validates_log_arguments_memory64() {
    validation(LOG64).await;
}

#[tokio::test]
async fn checks_use_the_current_memory_size() {
    let (rt, _) = setup();
    let wat = r#"
(module
  (import "orbit:log/v1" "write" (func $w (param i32 i64 i64) (result i32)))
  (memory (export "memory") 1 4)
  (func (export "log") (param i64 i64) (result i32) (call $w (i32.const 2) (local.get 0) (local.get 1)))
  (func (export "grow") (result i32) (memory.grow (i32.const 1))))"#;
    let mut iso = spawn(&rt, wat, "t", Limits::default()).await;
    let at = 65536 + 100;
    assert_eq!(
        iso.invoke("log", &[at, 4]).await.unwrap(),
        vec![i64::from(status::ERR_INVALID)]
    );
    assert_eq!(iso.invoke("grow", &[]).await.unwrap(), vec![1]);
    assert_eq!(iso.invoke("log", &[at, 4]).await.unwrap(), vec![0]);
}

#[tokio::test]
async fn log_byte_budget_is_per_invocation() {
    let (rt, sink) = setup();
    let mut iso = spawn(&rt, LOG32, "t", Limits::default().with_max_log_bytes(12)).await;
    // 5 + 5 fits, the third message would make 15 > 12.
    assert_eq!(iso.invoke("hello", &[]).await.unwrap(), vec![0]);
    assert_eq!(
        iso.invoke("hello", &[]).await.unwrap(),
        vec![0],
        "budget reset by a new invocation"
    );
    assert_eq!(status_of(&mut iso, 2, 16, 5).await, 0);
    assert_eq!(status_of(&mut iso, 2, 16, 5).await, 0);
    assert_eq!(sink.records().len(), 4);

    // Within one invocation: use a guest that logs three times.
    let wat = r#"
(module
  (import "orbit:log/v1" "write" (func $w (param i32 i64 i64) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "hello")
  (func (export "three") (result i32)
    (drop (call $w (i32.const 2) (i64.const 0) (i64.const 5)))
    (drop (call $w (i32.const 2) (i64.const 0) (i64.const 5)))
    (call $w (i32.const 2) (i64.const 0) (i64.const 5))))"#;
    let mut iso = spawn(&rt, wat, "t", Limits::default().with_max_log_bytes(12)).await;
    sink.take();
    assert_eq!(
        iso.invoke("three", &[]).await.unwrap(),
        vec![i64::from(status::ERR_QUOTA)]
    );
    assert_eq!(sink.records().len(), 2);
    let m = iso.metrics();
    assert_eq!(m.bytes_logged, 10, "rejected bytes are not counted");
    assert_eq!(m.host_calls[&CapabilityId::log_v1()], 3);
    // The next invocation gets a fresh budget.
    sink.take();
    assert_eq!(
        iso.invoke("three", &[]).await.unwrap(),
        vec![i64::from(status::ERR_QUOTA)]
    );
    assert_eq!(sink.records().len(), 2);
}

#[tokio::test]
async fn pointers_only_address_the_instances_own_memory() {
    // A guest's pointers only ever address its own memory: two instances logging the same
    // offsets see their own data.
    let (rt, sink) = setup();
    let mut a = spawn(&rt, LOG32, "a", Limits::default()).await;
    let mut b = spawn(&rt, LOG32, "b", Limits::default()).await;
    a.invoke("poke", &[16, i64::from(b'X')]).await.unwrap();
    a.invoke("hello", &[]).await.unwrap();
    b.invoke("hello", &[]).await.unwrap();
    let msgs: Vec<_> = sink
        .records()
        .into_iter()
        .map(|r| (r.tenant.to_string(), r.message))
        .collect();
    assert_eq!(
        msgs,
        [("a".into(), "Xello".into()), ("b".into(), "hello".into())]
    );
}
