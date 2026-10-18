use orbit_abi::{CapabilityId, status};
use orbit_caps::{InMemoryQueue, QueueBackend, QueueCapability, QueueError};
use orbit_runtime::{CapabilityRegistry, Grants, Isolate, Limits, Runtime, RuntimeConfig};
use serde_json::json;

/// Data: "orders" at 0 (6), "other" at 8 (5), "hello" at 16 (5), ff fe (bad utf-8 name) at 24,
/// a two-message batch at 32 (`02 00 00 00 "hi" 03 00 00 00 "abc"` = 13 bytes), a truncated
/// batch at 64 (`05 00 00 00 "ab"` = 6 bytes), a batch header claiming 2 MiB at 80.
fn wat(is64: bool) -> String {
    let (mem, off) = if is64 {
        ("(memory (export \"memory\") i64 20)", "(i64.const 0)")
    } else {
        ("(memory (export \"memory\") 20)", "(i32.const 0)")
    };
    let data = |at: u32, s: &str| {
        let o = if is64 {
            format!("(i64.const {at})")
        } else {
            format!("(i32.const {at})")
        };
        format!("(data {o} \"{s}\")")
    };
    format!(
        r#"(module
  (import "orbit:queue/v1" "publish" (func $p (param i64 i64 i64 i64) (result i32)))
  (import "orbit:queue/v1" "publish_batch" (func $b (param i64 i64 i64 i64) (result i32)))
  {mem}
  (data {off} "orders\00\00other\00\00\00hello\00\00\00\ff\fe")
  {d1}
  {d2}
  {d3}
  (func (export "publish") (param i64 i64 i64 i64) (result i32)
    (call $p (local.get 0) (local.get 1) (local.get 2) (local.get 3)))
  (func (export "publish_batch") (param i64 i64 i64 i64) (result i32)
    (call $b (local.get 0) (local.get 1) (local.get 2) (local.get 3))))"#,
        d1 = data(32, r"\02\00\00\00hi\03\00\00\00abc"),
        d2 = data(64, r"\05\00\00\00ab"),
        d3 = data(80, r"\00\00\20\00"),
    )
}

const ORDERS: [i64; 2] = [0, 6];
const OTHER: [i64; 2] = [8, 5];
const HELLO: [i64; 2] = [16, 5];
const BAD_UTF8: [i64; 2] = [24, 2];

async fn spawn(q: &InMemoryQueue, is64: bool, tenant: &str, grants: &Grants) -> Isolate {
    let rt = Runtime::new(
        RuntimeConfig::default(),
        CapabilityRegistry::new().with(QueueCapability::new(q.clone())),
    )
    .unwrap();
    let art = rt.load(wat(is64).as_bytes()).unwrap();
    let mut iso = rt
        .spawn_isolate(&art, tenant, grants, Limits::default())
        .await
        .unwrap();
    iso.init().await.unwrap();
    iso
}

fn grant(queues: &[&str]) -> Grants {
    Grants::none().grant_with(CapabilityId::queue_v1(), json!({ "publish": queues }))
}

async fn publish(iso: &mut Isolate, q: [i64; 2], b: [i64; 2]) -> i64 {
    iso.invoke("publish", &[q[0], q[1], b[0], b[1]])
        .await
        .unwrap()[0]
}

async fn call(iso: &mut Isolate, q: [i64; 2], b: [i64; 2]) -> i64 {
    iso.invoke("publish_batch", &[q[0], q[1], b[0], b[1]])
        .await
        .unwrap()[0]
}

fn code(c: i32) -> i64 {
    i64::from(c)
}

#[tokio::test]
async fn publish_reaches_the_backend_on_both_memory_widths() {
    for is64 in [false, true] {
        let q = InMemoryQueue::new();
        let mut iso = spawn(&q, is64, "t1", &grant(&["orders"])).await;
        assert_eq!(publish(&mut iso, ORDERS, HELLO).await, 0);
        assert_eq!(
            publish(&mut iso, ORDERS, [16, 0]).await,
            0,
            "empty body is allowed"
        );
        assert_eq!(q.messages("t1", "orders"), vec![b"hello".to_vec(), vec![]]);
    }
}

#[tokio::test]
async fn deny_by_default() {
    let q = InMemoryQueue::new();
    // Granted with no config, with an empty list, and with a different queue.
    for g in [
        Grants::none().grant(CapabilityId::queue_v1()),
        grant(&[]),
        grant(&["other"]),
    ] {
        let mut iso = spawn(&q, false, "t1", &g).await;
        assert_eq!(
            publish(&mut iso, ORDERS, HELLO).await,
            code(status::ERR_DENIED)
        );
    }
    // Not granted at all: the import is not linked.
    let rt = Runtime::new(
        RuntimeConfig::default(),
        CapabilityRegistry::new().with(QueueCapability::new(q.clone())),
    )
    .unwrap();
    let art = rt.load(wat(false).as_bytes()).unwrap();
    assert!(
        rt.spawn_isolate(&art, "t1", &Grants::none(), Limits::default())
            .await
            .is_err()
    );
    assert!(q.messages("t1", "orders").is_empty());
}

#[tokio::test]
async fn tenants_do_not_share_queues() {
    let q = InMemoryQueue::new();
    let mut a = spawn(&q, false, "a", &grant(&["orders"])).await;
    let mut b = spawn(&q, false, "b", &grant(&["orders"])).await;
    assert_eq!(publish(&mut a, ORDERS, HELLO).await, 0);
    assert_eq!(publish(&mut b, ORDERS, OTHER).await, 0);
    assert_eq!(q.messages("a", "orders"), vec![b"hello".to_vec()]);
    assert_eq!(q.messages("b", "orders"), vec![b"other".to_vec()]);
}

#[tokio::test]
async fn invalid_arguments() {
    let q = InMemoryQueue::new();
    let mut iso = spawn(&q, false, "t", &grant(&["orders", ""])).await;
    assert_eq!(
        publish(&mut iso, [0, 0], HELLO).await,
        code(status::ERR_INVALID),
        "empty name"
    );
    assert_eq!(
        publish(&mut iso, BAD_UTF8, HELLO).await,
        code(status::ERR_INVALID)
    );
    assert_eq!(
        publish(&mut iso, [0, 256], HELLO).await,
        code(status::ERR_TOO_LARGE)
    );
    assert_eq!(
        publish(&mut iso, ORDERS, [16, 2 * 1024 * 1024]).await,
        code(status::ERR_TOO_LARGE)
    );
    // Out-of-bounds pointers are a status, not a trap.
    assert_eq!(
        publish(&mut iso, [1 << 40, 6], HELLO).await,
        code(status::ERR_INVALID)
    );
    assert_eq!(
        publish(&mut iso, ORDERS, [1 << 40, 5]).await,
        code(status::ERR_INVALID)
    );
    assert!(q.messages("t", "orders").is_empty());
}

#[tokio::test]
async fn batch_roundtrip_and_malformed_batches() {
    let q = InMemoryQueue::new();
    let mut iso = spawn(&q, false, "t", &grant(&["orders"])).await;
    assert_eq!(call(&mut iso, ORDERS, [32, 13]).await, 0);
    assert_eq!(
        q.messages("t", "orders"),
        vec![b"hi".to_vec(), b"abc".to_vec()]
    );
    // Truncated frame, dangling header bytes, empty batch, and an oversized frame.
    assert_eq!(
        call(&mut iso, ORDERS, [64, 6]).await,
        code(status::ERR_INVALID)
    );
    assert_eq!(
        call(&mut iso, ORDERS, [32, 15]).await,
        code(status::ERR_INVALID)
    );
    assert_eq!(
        call(&mut iso, ORDERS, [32, 0]).await,
        code(status::ERR_INVALID)
    );
    assert_eq!(
        call(&mut iso, ORDERS, [80, 4]).await,
        code(status::ERR_TOO_LARGE)
    );
    assert_eq!(
        call(&mut iso, [8, 5], [32, 13]).await,
        code(status::ERR_DENIED)
    );
    assert_eq!(
        q.messages("t", "orders").len(),
        2,
        "rejected batches publish nothing"
    );
}

#[tokio::test]
async fn full_queue_is_quota_and_backend_failure_is_io() {
    let q = InMemoryQueue::with_capacity(1);
    let mut iso = spawn(&q, false, "t", &grant(&["orders"])).await;
    assert_eq!(publish(&mut iso, ORDERS, HELLO).await, 0);
    assert_eq!(
        publish(&mut iso, ORDERS, HELLO).await,
        code(status::ERR_QUOTA)
    );

    struct Broken;
    impl QueueBackend for Broken {
        fn publish(&self, _: &str, _: &str, _: &[u8]) -> Result<(), QueueError> {
            Err(QueueError::Backend("down".into()))
        }
    }
    let rt = Runtime::new(
        RuntimeConfig::default(),
        CapabilityRegistry::new().with(QueueCapability::new(Broken)),
    )
    .unwrap();
    let art = rt.load(wat(false).as_bytes()).unwrap();
    let mut iso = rt
        .spawn_isolate(&art, "t", &grant(&["orders"]), Limits::default())
        .await
        .unwrap();
    iso.init().await.unwrap();
    assert_eq!(publish(&mut iso, ORDERS, HELLO).await, code(status::ERR_IO));
    // The default publish_batch goes through publish.
    let r = iso.invoke("publish_batch", &[0, 6, 32, 13]).await.unwrap()[0];
    assert_eq!(r, code(status::ERR_IO));
}
