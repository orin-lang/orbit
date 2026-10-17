use orbit_abi::{CapabilityId, status};
use orbit_caps::RandomCapability;
use orbit_runtime::{
    CapabilityRegistry, Grants, InstantiateError, Isolate, Limits, Runtime, RuntimeConfig,
};

/// 32-bit memory (2 pages = 128 KiB). `fill` is a raw pass-through; `byte(addr)` reads memory.
const RANDOM32: &str = r#"
(module
  (import "orbit:random/v1" "fill" (func $fill (param i64 i64) (result i32)))
  (memory (export "memory") 2)
  (func (export "fill") (param i64 i64) (result i32) (call $fill (local.get 0) (local.get 1)))
  (func (export "byte") (param i64) (result i32) (i32.load8_u (i32.wrap_i64 (local.get 0)))))
"#;

const RANDOM64: &str = r#"
(module
  (import "orbit:random/v1" "fill" (func $fill (param i64 i64) (result i32)))
  (memory (export "memory") i64 2)
  (func (export "fill") (param i64 i64) (result i32) (call $fill (local.get 0) (local.get 1)))
  (func (export "byte") (param i64) (result i32) (i32.load8_u (local.get 0))))
"#;

fn runtime() -> Runtime {
    Runtime::new(
        RuntimeConfig::default(),
        CapabilityRegistry::new().with(RandomCapability::new()),
    )
    .unwrap()
}

async fn spawn(rt: &Runtime, wat: &str) -> Isolate {
    let art = rt.load(wat.as_bytes()).unwrap();
    let mut iso = rt
        .spawn_isolate(
            &art,
            "t",
            &Grants::none().grant(CapabilityId::random_v1()),
            Limits::default(),
        )
        .await
        .unwrap();
    iso.init().await.unwrap();
    iso
}

async fn read(iso: &mut Isolate, ptr: i64, len: i64) -> Vec<u8> {
    let mut v = Vec::new();
    for i in 0..len {
        v.push(iso.invoke("byte", &[ptr + i]).await.unwrap()[0] as u8);
    }
    v
}

fn rc(code: i32) -> Vec<i64> {
    vec![i64::from(code)]
}

#[tokio::test]
async fn fill_writes_random_bytes_and_calls_differ() {
    let rt = runtime();
    for wat in [RANDOM32, RANDOM64] {
        let mut iso = spawn(&rt, wat).await;
        assert_eq!(iso.invoke("fill", &[100, 32]).await.unwrap(), rc(0));
        let a = read(&mut iso, 100, 32).await;
        assert_eq!(iso.invoke("fill", &[100, 32]).await.unwrap(), rc(0));
        let b = read(&mut iso, 100, 32).await;
        assert_ne!(a, b);
        assert_ne!(a, vec![0; 32]);
        // Bytes outside the range are untouched.
        assert_eq!(read(&mut iso, 99, 1).await, vec![0]);
        assert_eq!(read(&mut iso, 132, 1).await, vec![0]);
        // Zero length is fine.
        assert_eq!(iso.invoke("fill", &[0, 0]).await.unwrap(), rc(0));
        assert_eq!(iso.metrics().host_calls[&CapabilityId::random_v1()], 3);
    }
}

#[tokio::test]
async fn fill_allows_exactly_64_kib_and_rejects_more() {
    let rt = runtime();
    let mut iso = spawn(&rt, RANDOM64).await;
    assert_eq!(iso.invoke("fill", &[0, 65536]).await.unwrap(), rc(0));
    assert_eq!(
        iso.invoke("fill", &[0, 65537]).await.unwrap(),
        rc(status::ERR_TOO_LARGE)
    );
    assert_eq!(
        iso.invoke("fill", &[0, i64::MAX]).await.unwrap(),
        rc(status::ERR_TOO_LARGE)
    );
}

#[tokio::test]
async fn fill_validates_bounds() {
    let rt = runtime();
    for wat in [RANDOM32, RANDOM64] {
        let mut iso = spawn(&rt, wat).await;
        let size = 2 * 65536;
        for (ptr, len) in [(size - 4, 8), (size, 1), (-1, 4), (0, -1), (i64::MAX, 2)] {
            let r = iso.invoke("fill", &[ptr, len]).await.unwrap();
            assert!(
                r == rc(status::ERR_INVALID) || r == rc(status::ERR_TOO_LARGE),
                "ptr={ptr} len={len} -> {r:?}"
            );
        }
        assert_eq!(
            iso.invoke("fill", &[i64::MAX, 2]).await.unwrap(),
            rc(status::ERR_INVALID)
        );
        // A failed call wrote nothing at the edge.
        assert_eq!(read(&mut iso, size - 4, 4).await, vec![0; 4]);
        // The last 8 bytes of memory are valid.
        assert_eq!(iso.invoke("fill", &[size - 8, 8]).await.unwrap(), rc(0));
    }
}

#[tokio::test]
async fn random_requires_a_grant() {
    let rt = runtime();
    let art = rt.load(RANDOM32.as_bytes()).unwrap();
    let e = rt
        .spawn_isolate(&art, "t", &Grants::none(), Limits::default())
        .await
        .unwrap_err();
    assert!(matches!(e, InstantiateError::NotGranted(_)), "{e}");
}
