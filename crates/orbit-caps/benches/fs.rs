//! `orbit:fs/v1` read throughput: a guest reads a 4 MiB file through stat/open/read/close.
//!
//! Run with `cargo bench -p orbit-caps`. See `docs/benchmarks.md`.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use orbit_abi::CapabilityId;
use orbit_caps::{FsCapability, FsGrant, MountMode};
use orbit_runtime::{CapabilityRegistry, Grants, Limits, Runtime, RuntimeConfig};

const FILE_BYTES: usize = 4 * 1024 * 1024;

const WAT: &str = r#"(module
  (import "orbit:fs/v1" "open" (func $open (param i64 i64 i32) (result i64)))
  (import "orbit:fs/v1" "read" (func $read (param i64 i64 i64) (result i64)))
  (import "orbit:fs/v1" "close" (func $close (param i64) (result i32)))
  (memory (export "memory") 32)
  (data (i32.const 0) "/data/f.bin")
  ;; Reads the whole file in `chunk`-byte calls; returns the byte count.
  (func (export "slurp") (param $chunk i64) (result i64)
    (local $h i64) (local $n i64) (local $total i64)
    (local.set $h (call $open (i64.const 0) (i64.const 11) (i32.const 0)))
    (loop $l
      (local.set $n (call $read (local.get $h) (i64.const 4096) (local.get $chunk)))
      (if (i64.gt_s (local.get $n) (i64.const 0))
        (then (local.set $total (i64.add (local.get $total) (local.get $n))) (br $l))))
    (drop (call $close (local.get $h)))
    (local.get $total)))"#;

fn bench_read(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("f.bin"), vec![0xA5u8; FILE_BYTES]).expect("write");
    let tk = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio");
    let rt = Runtime::new(
        RuntimeConfig::default(),
        CapabilityRegistry::new().with(FsCapability::new()),
    )
    .expect("runtime");
    let art = rt.load(WAT.as_bytes()).expect("load");
    let grants = Grants::none().grant_with(
        CapabilityId::fs_v1(),
        FsGrant::new()
            .mount("/data", dir.path(), MountMode::ReadOnly)
            .to_config()
            .expect("grant"),
    );
    let mut g = c.benchmark_group("fs_read");
    g.throughput(Throughput::Bytes(FILE_BYTES as u64));
    for chunk in [4096_i64, 65536, 1 << 20] {
        g.bench_function(format!("chunk_{chunk}"), |b| {
            let mut iso = tk.block_on(async {
                let mut iso = rt
                    .spawn_isolate(&art, "bench", &grants, Limits::default())
                    .await
                    .expect("spawn");
                iso.init().await.expect("init");
                iso
            });
            b.iter(|| {
                let r = tk.block_on(iso.invoke("slurp", &[chunk])).expect("slurp");
                assert_eq!(r[0], FILE_BYTES as i64);
            });
        });
    }
    g.finish();
}

criterion_group!(benches, bench_read);
criterion_main!(benches);
