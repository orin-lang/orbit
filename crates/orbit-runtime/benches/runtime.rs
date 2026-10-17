//! Orbit runtime benchmarks: artifact load, instance creation, warm invocation.
//!
//! Run with `cargo bench -p orbit-runtime`. See `docs/benchmarks.md`.

use std::fmt::Write as _;
use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use orbit_abi::CapabilityId;
use orbit_caps::{LogCapability, LogRecord, LogSink};
use orbit_runtime::{
    Artifact, CapabilityRegistry, EngineConfig, Grants, Limits, PoolingConfig, Runtime,
    RuntimeConfig,
};

/// Log sink that drops every record.
struct NullSink;

impl LogSink for NullSink {
    fn write(&self, _record: &LogRecord) {}
}

const MINIMAL: &str = r#"(module (memory (export "memory") 1) (func (export "noop")))"#;

const SMALL: &str = r#"(module
  (memory (export "memory") 1)
  (func (export "add") (param i64 i64) (result i64) local.get 0 local.get 1 i64.add))"#;

const COMPUTE: &str = r#"(module
  (memory (export "memory") 1)
  (func (export "add") (param i64 i64) (result i64) local.get 0 local.get 1 i64.add)
  (func $fib (export "fib") (param i64) (result i64)
    local.get 0 i64.const 2 i64.lt_s
    if (result i64)
      local.get 0
    else
      local.get 0 i64.const 1 i64.sub call $fib
      local.get 0 i64.const 2 i64.sub call $fib
      i64.add
    end)
  (func (export "fib_iter") (param i64) (result i64) (local i64 i64 i64)
    i64.const 1 local.set 2
    block
      loop
        local.get 0 i64.eqz br_if 1
        local.get 1 local.get 2 i64.add local.set 3
        local.get 2 local.set 1
        local.get 3 local.set 2
        local.get 0 i64.const 1 i64.sub local.set 0
        br 0
      end
    end
    local.get 1))"#;

const LOGGER: &str = r#"(module
  (import "orbit:log/v1" "write" (func $log (param i32 i64 i64) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "0123456789abcdef0123456789abcdef")
  (func (export "log32") (result i32) i32.const 2 i64.const 0 i64.const 32 call $log))"#;

/// A module with `n` small but non-trivial functions.
fn medium_wat(n: usize) -> String {
    let mut s = String::from("(module (memory (export \"memory\") 1)\n");
    for i in 0..n {
        let _ = writeln!(
            s,
            "(func (export \"f{i}\") (param i64 i64) (result i64) (local i64)
               local.get 0 local.get 1 i64.mul local.set 2
               (block (loop
                 local.get 2 i64.const {k} i64.gt_u br_if 1
                 local.get 2 i64.const {i} i64.add local.get 1 i64.xor local.set 2
                 br 0))
               local.get 2 local.get 0 i64.add)",
            k = i + 1000
        );
    }
    s.push(')');
    s
}

fn registry() -> CapabilityRegistry {
    CapabilityRegistry::new().with(LogCapability::new(NullSink))
}

fn runtime(pooling: bool) -> Runtime {
    runtime_with(pooling, false)
}

/// `metered`: compile fuel metering into guest code (ADR-0008). Off by default in Orbit.
fn runtime_with(pooling: bool, metered: bool) -> Runtime {
    let mut engine = EngineConfig {
        fuel_metering: metered,
        ..EngineConfig::default()
    };
    if pooling {
        engine.pooling = Some(PoolingConfig {
            max_instances: 64,
            max_memory_bytes: 16 * 1024 * 1024,
        });
    }
    Runtime::new(
        RuntimeConfig {
            engine,
            ..RuntimeConfig::default()
        },
        registry(),
    )
    .expect("runtime")
}

fn tokio_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio")
}

fn bench_load(c: &mut Criterion) {
    let rt = runtime(false);
    let mut g = c.benchmark_group("load");
    for (name, wat) in [
        ("small", SMALL.to_string()),
        ("medium_500fn", medium_wat(500)),
    ] {
        let wasm = wat::parse_str(&wat).expect("wat");
        // Cold: bypass the cache and compile every time.
        g.bench_function(format!("cold_{name}"), |b| {
            b.iter(|| Artifact::load(rt.engine(), rt.registry(), black_box(&wasm)).expect("load"));
        });
        // Hit: Runtime::load hashes the bytes and returns the cached artifact.
        rt.load(&wasm).expect("prime");
        g.bench_function(format!("cache_hit_{name}"), |b| {
            b.iter(|| rt.load(black_box(&wasm)).expect("load"));
        });
    }
    g.finish();
}

fn bench_instance(c: &mut Criterion) {
    let tk = tokio_rt();
    let mut g = c.benchmark_group("instance");
    for (name, pooling, metered) in [
        ("ondemand", false, false),
        ("pooling", true, false),
        ("ondemand_metered", false, true),
        ("pooling_metered", true, true),
    ] {
        let rt = runtime_with(pooling, metered);
        let art = rt.load(MINIMAL.as_bytes()).expect("load");
        g.bench_function(format!("minimal_{name}"), |b| {
            b.to_async(&tk).iter(|| async {
                let mut iso = rt
                    .spawn_isolate(&art, "bench", &Grants::none(), Limits::default())
                    .await
                    .expect("spawn");
                iso.init().await.expect("init");
                iso.terminate();
            });
        });
    }
    let rt = runtime(false);
    let art = rt.load(LOGGER.as_bytes()).expect("load");
    let grants = Grants::none().grant(CapabilityId::log_v1());
    g.bench_function("log_import_ondemand", |b| {
        b.to_async(&tk).iter(|| async {
            let mut iso = rt
                .spawn_isolate(&art, "bench", &grants, Limits::default())
                .await
                .expect("spawn");
            iso.init().await.expect("init");
            iso.terminate();
        });
    });
    g.finish();
}

fn bench_invoke(c: &mut Criterion) {
    let tk = tokio_rt();
    let rt = runtime(false);
    let grants = Grants::none().grant(CapabilityId::log_v1());
    let limits = Limits::default().with_max_log_bytes(u64::MAX / 2);
    let mut g = c.benchmark_group("invoke");

    // The same compute module on a metered engine (`Limits::fuel` unlimited: the cost is the
    // compiled-in accounting, not the budget). Compare `fib_rec_20` with `fib_rec_20_metered`.
    let rt_metered = runtime_with(false, true);
    let art_metered = rt_metered.load(COMPUTE.as_bytes()).expect("load");
    let iso_metered = tokio::sync::Mutex::new(tk.block_on(async {
        let mut i = rt_metered
            .spawn_isolate(&art_metered, "bench", &grants, limits.clone())
            .await
            .expect("spawn");
        i.init().await.expect("init");
        i
    }));
    for (name, arg) in [("fib_rec_20_metered", 20), ("fib_iter_25_metered", 25)] {
        let func = if name.starts_with("fib_rec") {
            "fib"
        } else {
            "fib_iter"
        };
        g.bench_function(name, |b| {
            b.to_async(&tk).iter(|| async {
                iso_metered
                    .lock()
                    .await
                    .invoke(func, &[black_box(arg)])
                    .await
                    .expect("invoke")
            });
        });
    }

    let art = rt.load(COMPUTE.as_bytes()).expect("load");
    let iso = tokio::sync::Mutex::new(tk.block_on(async {
        let mut i = rt
            .spawn_isolate(&art, "bench", &grants, limits.clone())
            .await
            .expect("spawn");
        i.init().await.expect("init");
        i
    }));
    g.bench_function("add", |b| {
        b.to_async(&tk).iter(|| async {
            iso.lock()
                .await
                .invoke("add", &[black_box(1), black_box(2)])
                .await
                .expect("invoke")
        });
    });
    g.bench_function("fib_iter_25", |b| {
        b.to_async(&tk).iter(|| async {
            iso.lock()
                .await
                .invoke("fib_iter", &[black_box(25)])
                .await
                .expect("invoke")
        });
    });
    g.bench_function("fib_rec_20", |b| {
        b.to_async(&tk).iter(|| async {
            iso.lock()
                .await
                .invoke("fib", &[black_box(20)])
                .await
                .expect("invoke")
        });
    });

    let art = rt.load(LOGGER.as_bytes()).expect("load");
    let log_iso = tokio::sync::Mutex::new(tk.block_on(async {
        let mut i = rt
            .spawn_isolate(&art, "bench", &grants, limits.clone())
            .await
            .expect("spawn");
        i.init().await.expect("init");
        i
    }));
    g.bench_function("host_call_log_32b", |b| {
        b.to_async(&tk).iter(|| async {
            let r = log_iso
                .lock()
                .await
                .invoke("log32", &[])
                .await
                .expect("invoke");
            debug_assert_eq!(r, vec![0]);
            r
        });
    });
    g.finish();
}

criterion_group!(benches, bench_load, bench_instance, bench_invoke);
criterion_main!(benches);
