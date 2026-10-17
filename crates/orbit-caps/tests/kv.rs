use std::sync::Arc;

use orbit_abi::{CapabilityId, status};
use orbit_caps::{InMemoryKv, KvCapability, KvStore};
use orbit_runtime::{
    CapabilityRegistry, Grants, InstantiateError, Isolate, Limits, Runtime, RuntimeConfig,
};
use serde_json::json;

/// Builds a guest around the five kv imports. Data: "cache" at 0, "other" at 8, "k1" at 16,
/// "hello" at 24, bytes ff fe at 32. Memory is 20 pages (1.25 MiB). Exports are raw
/// pass-throughs plus `byte(addr)` / `poke(addr, b)`.
fn kv_wat(is64: bool) -> String {
    let (mem, off, load, store) = if is64 {
        (
            "(memory (export \"memory\") i64 20)",
            "(i64.const 0)",
            "(i32.load8_u (local.get 0))",
            "(i32.store8 (local.get 0) (local.get 1))",
        )
    } else {
        (
            "(memory (export \"memory\") 20)",
            "(i32.const 0)",
            "(i32.load8_u (i32.wrap_i64 (local.get 0)))",
            "(i32.store8 (i32.wrap_i64 (local.get 0)) (local.get 1))",
        )
    };
    format!(
        r#"(module
  (import "orbit:kv/v1" "open" (func $open (param i64 i64) (result i64)))
  (import "orbit:kv/v1" "get" (func $get (param i64 i64 i64 i64 i64) (result i64)))
  (import "orbit:kv/v1" "put" (func $put (param i64 i64 i64 i64 i64) (result i32)))
  (import "orbit:kv/v1" "delete" (func $del (param i64 i64 i64) (result i32)))
  (import "orbit:kv/v1" "close" (func $close (param i64) (result i32)))
  {mem}
  (data {off} "cache\00\00\00other\00\00\00k1\00\00\00\00\00\00hello\00\00\00\ff\fe")
  (func (export "open") (param i64 i64) (result i64) (call $open (local.get 0) (local.get 1)))
  (func (export "get") (param i64 i64 i64 i64 i64) (result i64)
    (call $get (local.get 0) (local.get 1) (local.get 2) (local.get 3) (local.get 4)))
  (func (export "put") (param i64 i64 i64 i64 i64) (result i32)
    (call $put (local.get 0) (local.get 1) (local.get 2) (local.get 3) (local.get 4)))
  (func (export "delete") (param i64 i64 i64) (result i32)
    (call $del (local.get 0) (local.get 1) (local.get 2)))
  (func (export "close") (param i64) (result i32) (call $close (local.get 0)))
  (func (export "byte") (param i64) (result i32) {load})
  (func (export "poke") (param i64 i32) {store}))"#
    )
}

const CACHE: [i64; 2] = [0, 5];
const OTHER: [i64; 2] = [8, 5];
const K1: [i64; 2] = [16, 2];
const HELLO: [i64; 2] = [24, 5];
const BAD_UTF8: [i64; 2] = [32, 2];
const OUT: i64 = 4096;

struct Fixture {
    rt: Runtime,
    store: InMemoryKv,
}

fn fixture() -> Fixture {
    let store = InMemoryKv::new();
    let rt = Runtime::new(
        RuntimeConfig::default(),
        CapabilityRegistry::new().with(KvCapability::new(store.clone())),
    )
    .unwrap();
    Fixture { rt, store }
}

fn grants(namespaces: &[&str]) -> Grants {
    Grants::none().grant_with(CapabilityId::kv_v1(), json!({ "namespaces": namespaces }))
}

async fn spawn_with(f: &Fixture, wat: &str, tenant: &str, g: &Grants) -> Isolate {
    let art = f.rt.load(wat.as_bytes()).unwrap();
    let mut iso =
        f.rt.spawn_isolate(&art, tenant, g, Limits::default())
            .await
            .unwrap();
    iso.init().await.unwrap();
    iso
}

async fn spawn(f: &Fixture, tenant: &str) -> Isolate {
    spawn_with(f, &kv_wat(false), tenant, &grants(&["cache"])).await
}

async fn call(iso: &mut Isolate, f: &str, args: &[i64]) -> i64 {
    iso.invoke(f, args).await.unwrap()[0]
}

async fn open(iso: &mut Isolate, name: [i64; 2]) -> i64 {
    call(iso, "open", &name).await
}

async fn put(iso: &mut Isolate, h: i64, key: [i64; 2], val: [i64; 2]) -> i64 {
    call(iso, "put", &[h, key[0], key[1], val[0], val[1]]).await
}

async fn get(iso: &mut Isolate, h: i64, key: [i64; 2], cap: i64) -> i64 {
    call(iso, "get", &[h, key[0], key[1], OUT, cap]).await
}

async fn bytes(iso: &mut Isolate, ptr: i64, len: i64) -> Vec<u8> {
    let mut v = Vec::new();
    for i in 0..len {
        v.push(call(iso, "byte", &[ptr + i]).await as u8);
    }
    v
}

fn code(c: i32) -> i64 {
    i64::from(c)
}

#[tokio::test]
async fn put_get_roundtrip_and_delete_on_both_memory_widths() {
    let f = fixture();
    for is64 in [false, true] {
        let tenant = if is64 { "t64" } else { "t32" };
        let mut iso = spawn_with(&f, &kv_wat(is64), tenant, &grants(&["cache"])).await;
        let h = open(&mut iso, CACHE).await;
        assert!(h > 0, "{h}");
        assert_eq!(get(&mut iso, h, K1, 64).await, code(status::ERR_NOT_FOUND));
        assert_eq!(put(&mut iso, h, K1, HELLO).await, 0);
        assert_eq!(get(&mut iso, h, K1, 64).await, 5);
        assert_eq!(bytes(&mut iso, OUT, 5).await, b"hello");
        // Overwrite with a different value (key bytes as the value).
        assert_eq!(put(&mut iso, h, K1, [0, 5]).await, 0);
        assert_eq!(get(&mut iso, h, K1, 5).await, 5);
        assert_eq!(bytes(&mut iso, OUT, 5).await, b"cache");
        assert_eq!(call(&mut iso, "delete", &[h, K1[0], K1[1]]).await, 0);
        assert_eq!(get(&mut iso, h, K1, 64).await, code(status::ERR_NOT_FOUND));
        // Deleting an absent key is idempotent.
        assert_eq!(call(&mut iso, "delete", &[h, K1[0], K1[1]]).await, 0);
        assert_eq!(call(&mut iso, "close", &[h]).await, 0);
        // open, get x3, put x2, get... counted per call
        assert!(iso.metrics().host_calls[&CapabilityId::kv_v1()] >= 10);
    }
}

#[tokio::test]
async fn small_buffer_returns_length_without_writing() {
    let f = fixture();
    let mut iso = spawn(&f, "t").await;
    let h = open(&mut iso, CACHE).await;
    assert_eq!(put(&mut iso, h, K1, HELLO).await, 0);
    // Pre-fill the out buffer so we can tell whether it was touched.
    for i in 0..5 {
        iso.invoke("poke", &[OUT + i, 0x55]).await.unwrap();
    }
    assert_eq!(get(&mut iso, h, K1, 4).await, 5);
    assert_eq!(get(&mut iso, h, K1, 0).await, 5);
    assert_eq!(bytes(&mut iso, OUT, 5).await, vec![0x55; 5]);
    // Exactly enough copies.
    assert_eq!(get(&mut iso, h, K1, 5).await, 5);
    assert_eq!(bytes(&mut iso, OUT, 5).await, b"hello");
    // A negative capacity is invalid.
    assert_eq!(get(&mut iso, h, K1, -1).await, code(status::ERR_INVALID));
}

#[tokio::test]
async fn namespace_must_be_granted_and_valid() {
    let f = fixture();
    let mut iso = spawn(&f, "t").await;
    assert_eq!(open(&mut iso, OTHER).await, code(status::ERR_DENIED));
    assert_eq!(open(&mut iso, BAD_UTF8).await, code(status::ERR_INVALID));
    assert_eq!(open(&mut iso, [0, 0]).await, code(status::ERR_DENIED));
    assert_eq!(
        open(&mut iso, [1 << 21, 5]).await,
        code(status::ERR_INVALID)
    );
    assert_eq!(open(&mut iso, [-1, 5]).await, code(status::ERR_INVALID));
    assert_eq!(open(&mut iso, [0, 2000]).await, code(status::ERR_TOO_LARGE));
    assert!(open(&mut iso, CACHE).await > 0);

    // A grant without a namespace list (or with a malformed one) allows nothing.
    for g in [
        Grants::none().grant(CapabilityId::kv_v1()),
        Grants::none().grant_with(CapabilityId::kv_v1(), json!({"namespaces": "cache"})),
        grants(&[]),
    ] {
        let mut iso = spawn_with(&f, &kv_wat(false), "t", &g).await;
        assert_eq!(open(&mut iso, CACHE).await, code(status::ERR_DENIED));
    }
    // A grant for several namespaces allows each.
    let mut iso = spawn_with(&f, &kv_wat(false), "t", &grants(&["cache", "other"])).await;
    assert!(open(&mut iso, CACHE).await > 0);
    assert!(open(&mut iso, OTHER).await > 0);
}

#[tokio::test]
async fn namespaces_are_separate_storage() {
    let f = fixture();
    let mut iso = spawn_with(&f, &kv_wat(false), "t", &grants(&["cache", "other"])).await;
    let a = open(&mut iso, CACHE).await;
    let b = open(&mut iso, OTHER).await;
    assert_eq!(put(&mut iso, a, K1, HELLO).await, 0);
    assert_eq!(get(&mut iso, b, K1, 8).await, code(status::ERR_NOT_FOUND));
    assert_eq!(f.store.get("t", "cache", b"k1").unwrap().unwrap(), b"hello");
}

#[tokio::test]
async fn tenants_are_separated_but_instances_of_one_tenant_share() {
    let f = fixture();
    let mut a1 = spawn(&f, "acme").await;
    let mut a2 = spawn(&f, "acme").await;
    let mut g1 = spawn(&f, "globex").await;
    let (h_a1, h_a2, h_g1) = (
        open(&mut a1, CACHE).await,
        open(&mut a2, CACHE).await,
        open(&mut g1, CACHE).await,
    );
    assert_eq!(put(&mut a1, h_a1, K1, HELLO).await, 0);
    // Same tenant, other instance: shared.
    assert_eq!(get(&mut a2, h_a2, K1, 16).await, 5);
    assert_eq!(bytes(&mut a2, OUT, 5).await, b"hello");
    // Other tenant, same namespace and key: invisible.
    assert_eq!(
        get(&mut g1, h_g1, K1, 16).await,
        code(status::ERR_NOT_FOUND)
    );
    // Same key, different value in the other tenant does not clobber.
    assert_eq!(put(&mut g1, h_g1, K1, CACHE).await, 0);
    assert_eq!(get(&mut a1, h_a1, K1, 16).await, 5);
    assert_eq!(bytes(&mut a1, OUT, 5).await, b"hello");
    assert_eq!(
        f.store.get("globex", "cache", b"k1").unwrap().unwrap(),
        b"cache"
    );
    // Deleting in one tenant leaves the other alone.
    assert_eq!(call(&mut g1, "delete", &[h_g1, K1[0], K1[1]]).await, 0);
    assert_eq!(get(&mut a2, h_a2, K1, 16).await, 5);
}

#[tokio::test]
async fn handles_are_positive_per_instance_and_not_forgeable_across_instances() {
    let f = fixture();
    let mut a = spawn(&f, "t").await;
    let mut b = spawn(&f, "t").await;
    let ha = open(&mut a, CACHE).await;
    assert!(ha > 0);
    // B has no handle at all: A's number is rejected.
    assert_eq!(get(&mut b, ha, K1, 8).await, code(status::ERR_BAD_HANDLE));
    assert_eq!(
        put(&mut b, ha, K1, HELLO).await,
        code(status::ERR_BAD_HANDLE)
    );
    assert_eq!(
        call(&mut b, "close", &[ha]).await,
        code(status::ERR_BAD_HANDLE)
    );
    // B has a handle in the very same slot with the same generation: still rejected.
    let hb = open(&mut b, CACHE).await;
    assert_ne!(ha, hb);
    assert_eq!(
        get(&mut b, ha, K1, 8).await,
        code(status::ERR_NOT_FOUND).min(code(status::ERR_BAD_HANDLE))
    );
    assert_eq!(
        call(&mut a, "close", &[hb]).await,
        code(status::ERR_BAD_HANDLE)
    );
    // A's handle still works for A.
    assert_eq!(put(&mut a, ha, K1, HELLO).await, 0);
    // Guessing: zero, negatives and small numbers are all bad handles.
    for bad in [0, -1, 1, 2, 3, i64::MAX, i64::MIN] {
        assert_eq!(
            get(&mut a, bad, K1, 8).await,
            code(status::ERR_BAD_HANDLE),
            "{bad}"
        );
    }
}

#[tokio::test]
async fn closed_handles_and_generations() {
    let f = fixture();
    let mut iso = spawn(&f, "t").await;
    let h1 = open(&mut iso, CACHE).await;
    assert_eq!(call(&mut iso, "close", &[h1]).await, 0);
    // Use after close, and double close.
    assert_eq!(get(&mut iso, h1, K1, 8).await, code(status::ERR_BAD_HANDLE));
    assert_eq!(
        put(&mut iso, h1, K1, HELLO).await,
        code(status::ERR_BAD_HANDLE)
    );
    assert_eq!(
        call(&mut iso, "delete", &[h1, K1[0], K1[1]]).await,
        code(status::ERR_BAD_HANDLE)
    );
    assert_eq!(
        call(&mut iso, "close", &[h1]).await,
        code(status::ERR_BAD_HANDLE)
    );
    // Reopening reuses the slot but the stale handle must not alias the new one.
    let h2 = open(&mut iso, CACHE).await;
    assert!(h2 > 0 && h2 != h1);
    assert_eq!(get(&mut iso, h1, K1, 8).await, code(status::ERR_BAD_HANDLE));
    assert_eq!(
        call(&mut iso, "close", &[h1]).await,
        code(status::ERR_BAD_HANDLE)
    );
    assert_eq!(get(&mut iso, h2, K1, 8).await, code(status::ERR_NOT_FOUND));
    assert_eq!(put(&mut iso, h2, K1, HELLO).await, 0);
}

#[tokio::test]
async fn open_handles_are_limited_per_instance() {
    let f = fixture();
    let mut iso = spawn(&f, "t").await;
    let mut hs = Vec::new();
    for _ in 0..64 {
        let h = open(&mut iso, CACHE).await;
        assert!(h > 0);
        hs.push(h);
    }
    assert_eq!(open(&mut iso, CACHE).await, code(status::ERR_QUOTA));
    // Another instance has its own budget.
    let mut other = spawn(&f, "t").await;
    assert!(open(&mut other, CACHE).await > 0);
    // Closing one frees a slot.
    assert_eq!(call(&mut iso, "close", &[hs[10]]).await, 0);
    assert!(open(&mut iso, CACHE).await > 0);
    assert_eq!(open(&mut iso, CACHE).await, code(status::ERR_QUOTA));
    // Denied/invalid opens do not consume budget or change the quota answer.
    assert_eq!(open(&mut iso, OTHER).await, code(status::ERR_DENIED));
}

#[tokio::test]
async fn size_limits_and_bounds() {
    let f = fixture();
    for is64 in [false, true] {
        let mut iso = spawn_with(&f, &kv_wat(is64), "t", &grants(&["cache"])).await;
        let h = open(&mut iso, CACHE).await;
        let big_key = 1025;
        assert_eq!(
            put(&mut iso, h, [0, big_key], HELLO).await,
            code(status::ERR_TOO_LARGE)
        );
        assert_eq!(
            get(&mut iso, h, [0, big_key], 8).await,
            code(status::ERR_TOO_LARGE)
        );
        assert_eq!(
            call(&mut iso, "delete", &[h, 0, big_key]).await,
            code(status::ERR_TOO_LARGE)
        );
        // A key of exactly 1 KiB is fine.
        assert_eq!(put(&mut iso, h, [0, 1024], HELLO).await, 0);
        assert_eq!(get(&mut iso, h, [0, 1024], 8).await, 5);
        // Values: 1 MiB fits, one more byte does not.
        let mib = 1024 * 1024;
        assert_eq!(put(&mut iso, h, K1, [0, mib]).await, 0);
        assert_eq!(get(&mut iso, h, K1, 0).await, mib);
        assert_eq!(
            put(&mut iso, h, K1, [0, mib + 1]).await,
            code(status::ERR_TOO_LARGE)
        );
        // The failed put left the old value.
        assert_eq!(get(&mut iso, h, K1, 0).await, mib);
        // Out-of-bounds and bad arguments.
        let mem = 20 * 65536;
        assert_eq!(
            put(&mut iso, h, K1, [mem - 2, 5]).await,
            code(status::ERR_INVALID)
        );
        assert_eq!(
            put(&mut iso, h, [mem, 1], HELLO).await,
            code(status::ERR_INVALID)
        );
        assert_eq!(
            put(&mut iso, h, [0, 0], HELLO).await,
            code(status::ERR_INVALID)
        );
        assert_eq!(
            put(&mut iso, h, K1, [-1, 5]).await,
            code(status::ERR_INVALID)
        );
        assert_eq!(
            put(&mut iso, h, K1, [i64::MAX, 5]).await,
            code(status::ERR_INVALID)
        );
        // A get whose output buffer is out of bounds fails instead of writing.
        assert_eq!(
            call(&mut iso, "get", &[h, K1[0], K1[1], mem - 2, mib]).await,
            code(status::ERR_INVALID)
        );
        assert_eq!(
            call(&mut iso, "get", &[h, K1[0], K1[1], i64::MAX, mib]).await,
            code(status::ERR_INVALID)
        );
    }
}

#[tokio::test]
async fn kv_without_a_grant_cannot_instantiate() {
    let f = fixture();
    let art = f.rt.load(kv_wat(false).as_bytes()).unwrap();
    let e =
        f.rt.spawn_isolate(&art, "t", &Grants::none(), Limits::default())
            .await
            .unwrap_err();
    assert!(
        matches!(e, InstantiateError::NotGranted(ref id) if *id == CapabilityId::kv_v1()),
        "{e}"
    );
    // Granting only other capabilities does not help.
    let other = Grants::none().grant(CapabilityId::log_v1());
    assert!(
        f.rt.spawn_isolate(&art, "t", &other, Limits::default())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn terminate_drops_handles_and_data_survives() {
    let f = fixture();
    let mut a = spawn(&f, "t").await;
    let h = open(&mut a, CACHE).await;
    assert_eq!(put(&mut a, h, K1, HELLO).await, 0);
    a.terminate();
    assert!(a.invoke("get", &[h, K1[0], K1[1], OUT, 8]).await.is_err());
    // Quota is per instance, so a fresh instance starts empty and sees the stored data.
    let mut b = spawn(&f, "t").await;
    let hb = open(&mut b, CACHE).await;
    assert_eq!(get(&mut b, hb, K1, 8).await, 5);
}

/// A failing adapter maps to ERR_IO.
#[tokio::test]
async fn adapter_failure_is_err_io() {
    struct Broken;
    impl KvStore for Broken {
        fn get(&self, _: &str, _: &str, _: &[u8]) -> Result<Option<Vec<u8>>, orbit_caps::KvError> {
            Err(orbit_caps::KvError("down".into()))
        }
        fn put(&self, _: &str, _: &str, _: &[u8], _: &[u8]) -> Result<(), orbit_caps::KvError> {
            Err(orbit_caps::KvError("down".into()))
        }
        fn delete(&self, _: &str, _: &str, _: &[u8]) -> Result<(), orbit_caps::KvError> {
            Err(orbit_caps::KvError("down".into()))
        }
    }
    let rt = Runtime::new(
        RuntimeConfig::default(),
        CapabilityRegistry::new().with(KvCapability::with_shared_store(Arc::new(Broken))),
    )
    .unwrap();
    let f = Fixture {
        rt,
        store: InMemoryKv::new(),
    };
    let mut iso = spawn(&f, "t").await;
    let h = open(&mut iso, CACHE).await;
    assert_eq!(put(&mut iso, h, K1, HELLO).await, code(status::ERR_IO));
    assert_eq!(get(&mut iso, h, K1, 8).await, code(status::ERR_IO));
    assert_eq!(
        call(&mut iso, "delete", &[h, K1[0], K1[1]]).await,
        code(status::ERR_IO)
    );
}
