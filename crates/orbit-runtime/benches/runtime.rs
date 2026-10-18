//! Orbit runtime benchmarks: artifact load, instance creation, warm invocation.
//!
//! Run with `cargo bench -p orbit-runtime`. See `docs/benchmarks.md`.

use std::fmt::Write as _;
use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use orbit_abi::CapabilityId;
use orbit_caps::{LogCapability, LogRecord, LogSink};
use orbit_runtime::{
    Artifact, CapabilityRegistry, EngineConfig, Grants, Limits, PoolConfig, PoolingConfig,
    ReusePolicy, Runtime, RuntimeConfig,
};
use std::time::{Duration, Instant};

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

/// Stands in for a program with real start-up work: `orbit_init` touches 16 pages and runs a
/// 200k-iteration loop (a few hundred microseconds), then `add` is trivial.
const HEAVY_INIT: &str = r#"(module
  (memory (export "memory") 17)
  (func (export "orbit_init") (result i32) (local $i i32)
    (memory.fill (i32.const 0) (i32.const 1) (i32.const 1048576))
    (loop $l
      (i32.store (i32.and (i32.mul (local.get $i) (i32.const 64)) (i32.const 1048575)) (local.get $i))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $l (i32.lt_u (local.get $i) (i32.const 200000))))
    (i32.const 0))
  (func (export "add") (param i64 i64) (result i64) local.get 0 local.get 1 i64.add))"#;

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
            ..PoolingConfig::default()
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

/// Per-request cost of the three ways to get a ready isolate and run one call on it:
/// spawn + init + invoke + terminate, a prewarmed `FreshPerAcquire` pool (acquire hit +
/// invoke + release, where release terminates), and a `Reuse` pool. Also pool misses (the
/// acquire creates the isolate itself), on both allocators.
fn bench_pool(c: &mut Criterion) {
    const CHUNK: u64 = 128;
    let tk = tokio_rt();
    // `Runtime::pool` spawns the pool's background task: it needs a Tokio context.
    let _enter = tk.enter();
    let limits = Limits::default().with_max_memory(1024 * 1024);
    let mut g = c.benchmark_group("pool");
    for (alloc, pooling) in [("ondemand", false), ("pooling", true)] {
        let mut engine = EngineConfig::default();
        if pooling {
            engine.pooling = Some(PoolingConfig {
                max_instances: 512,
                max_memory_bytes: 1024 * 1024,
                ..PoolingConfig::default()
            });
        }
        let rt = Runtime::new(
            RuntimeConfig {
                engine,
                ..RuntimeConfig::default()
            },
            registry(),
        )
        .expect("runtime");
        let art = rt.load(COMPUTE.as_bytes()).expect("load");

        // Baseline: no pool.
        g.bench_function(format!("spawn_init_invoke_terminate_{alloc}"), |b| {
            b.to_async(&tk).iter(|| async {
                let mut iso = rt
                    .spawn_isolate(&art, "bench", &Grants::none(), limits.clone())
                    .await
                    .expect("spawn");
                iso.init().await.expect("init");
                let r = iso.invoke("add", &[black_box(1), black_box(2)]).await;
                iso.terminate();
                r.expect("invoke")
            });
        });

        // Fresh isolate per acquire, prewarmed outside the timed region (chunks of CHUNK).
        let fresh = rt
            .pool(
                &art,
                "bench",
                &Grants::none(),
                limits.clone(),
                PoolConfig::default()
                    .with_max_idle(CHUNK as usize)
                    .with_max_total(CHUNK as usize * 2)
                    .with_idle_timeout(None),
            )
            .expect("pool");
        g.bench_function(format!("fresh_hit_acquire_invoke_release_{alloc}"), |b| {
            b.to_async(&tk).iter_custom(|iters| {
                let fresh = fresh.clone();
                async move {
                    let mut total = Duration::ZERO;
                    let mut left = iters;
                    while left > 0 {
                        let n = left.min(CHUNK);
                        fresh.prewarm(n as usize).await.expect("prewarm");
                        let t = Instant::now();
                        for _ in 0..n {
                            let mut iso = fresh.acquire().await.expect("acquire");
                            black_box(iso.invoke("add", &[1, 2]).await.expect("invoke"));
                        }
                        total += t.elapsed();
                        left -= n;
                    }
                    total
                }
            });
        });

        // Miss: nothing idle, the acquire spawns and initialises (pool overhead over the baseline).
        let cold = rt
            .pool(
                &art,
                "bench",
                &Grants::none(),
                limits.clone(),
                PoolConfig::default().with_idle_timeout(None),
            )
            .expect("pool");
        g.bench_function(format!("fresh_miss_acquire_invoke_release_{alloc}"), |b| {
            b.to_async(&tk).iter(|| async {
                let mut iso = cold.acquire().await.expect("acquire");
                iso.invoke("add", &[black_box(1), black_box(2)])
                    .await
                    .expect("invoke")
            });
        });

        // Reuse: the same isolate serves every request.
        let reuse = rt
            .pool(
                &art,
                "bench",
                &Grants::none(),
                limits.clone(),
                PoolConfig::default()
                    .with_reuse(ReusePolicy::Reuse { max_uses: u64::MAX })
                    .with_idle_timeout(None),
            )
            .expect("pool");
        g.bench_function(format!("reuse_acquire_invoke_release_{alloc}"), |b| {
            b.to_async(&tk).iter(|| async {
                let mut iso = reuse.acquire().await.expect("acquire");
                iso.invoke("add", &[black_box(1), black_box(2)])
                    .await
                    .expect("invoke")
            });
        });
    }

    // A program with real start-up work: this is where a pool pays off.
    {
        let rt = runtime(false);
        let art = rt.load(HEAVY_INIT.as_bytes()).expect("load");
        let limits = Limits::default().with_max_memory(2 * 1024 * 1024);
        g.bench_function("heavy_init_spawn_init_invoke_terminate", |b| {
            b.to_async(&tk).iter(|| async {
                let mut iso = rt
                    .spawn_isolate(&art, "bench", &Grants::none(), limits.clone())
                    .await
                    .expect("spawn");
                iso.init().await.expect("init");
                let r = iso.invoke("add", &[black_box(1), black_box(2)]).await;
                iso.terminate();
                r.expect("invoke")
            });
        });
        let fresh = rt
            .pool(
                &art,
                "bench",
                &Grants::none(),
                limits.clone(),
                PoolConfig::default()
                    .with_max_idle(CHUNK as usize)
                    .with_idle_timeout(None),
            )
            .expect("pool");
        g.bench_function("heavy_init_fresh_hit_acquire_invoke_release", |b| {
            b.to_async(&tk).iter_custom(|iters| {
                let fresh = fresh.clone();
                async move {
                    let mut total = Duration::ZERO;
                    let mut left = iters;
                    while left > 0 {
                        let n = left.min(CHUNK);
                        fresh.prewarm(n as usize).await.expect("prewarm");
                        let t = Instant::now();
                        for _ in 0..n {
                            let mut iso = fresh.acquire().await.expect("acquire");
                            black_box(iso.invoke("add", &[1, 2]).await.expect("invoke"));
                        }
                        total += t.elapsed();
                        left -= n;
                    }
                    total
                }
            });
        });
    }

    // Allocator tuning: spawn+init+terminate with slots kept resident or not.
    for (name, cfg) in [
        ("pooling_density", PoolingConfig::density(64)),
        ("pooling_latency", PoolingConfig::latency(64)),
    ] {
        let rt = Runtime::new(
            RuntimeConfig {
                engine: EngineConfig {
                    pooling: Some(PoolingConfig {
                        max_memory_bytes: 16 * 1024 * 1024,
                        ..cfg
                    }),
                    ..EngineConfig::default()
                },
                ..RuntimeConfig::default()
            },
            registry(),
        )
        .expect("runtime");
        let art = rt.load(SMALL.as_bytes()).expect("load");
        g.bench_function(format!("spawn_init_invoke_terminate_{name}"), |b| {
            b.to_async(&tk).iter(|| async {
                let mut iso = rt
                    .spawn_isolate(&art, "bench", &Grants::none(), Limits::default())
                    .await
                    .expect("spawn");
                iso.init().await.expect("init");
                let r = iso.invoke("add", &[black_box(1), black_box(2)]).await;
                iso.terminate();
                r.expect("invoke")
            });
        });
    }
    g.finish();
}

criterion_group!(
    benches,
    bench_load,
    bench_instance,
    bench_invoke,
    bench_pool
);
criterion_main!(benches);
