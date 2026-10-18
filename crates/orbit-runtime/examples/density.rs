//! Isolate density: spawn N idle, initialised isolates and report memory and spawn latency.
//!
//! Usage: `cargo run --release -p orbit-runtime --example density -- [N] [--pooling] [--pool]`
//!
//! `--pool` keeps the N isolates idle in an [`InstancePool`](orbit_runtime::InstancePool)
//! instead, then measures acquire latency (a fresh, prewarmed isolate per request) and the
//! effect of `Runtime::reclaim_idle`.
//!
//! Memory is the process resident set (`VmRSS` in `/proc/self/status`) before and after, so
//! it includes everything the process allocated while spawning, not only guest memory.

use std::time::{Duration, Instant};

use orbit_runtime::{
    CapabilityRegistry, EngineConfig, Grants, Limits, PoolConfig, PoolingConfig, Runtime,
    RuntimeConfig,
};

const MODULE: &str = r#"(module (memory (export "memory") 1) (func (export "orbit_init") (result i32) i32.const 0))"#;

fn rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx]
}

#[tokio::main]
async fn main() {
    let mut n: usize = 1000;
    let mut pooling = false;
    let mut pool = false;
    for arg in std::env::args().skip(1) {
        if arg == "--pooling" {
            pooling = true;
        } else if arg == "--pool" {
            pool = true;
        } else if let Ok(v) = arg.parse() {
            n = v;
        } else {
            eprintln!("usage: density [N] [--pooling] [--pool]");
            std::process::exit(2);
        }
    }
    let mut engine = EngineConfig::default();
    if pooling {
        engine.pooling = Some(PoolingConfig {
            max_instances: u32::try_from(n).expect("N fits u32"),
            max_memory_bytes: 1024 * 1024,
            ..PoolingConfig::default()
        });
    }
    let rt = Runtime::new(
        RuntimeConfig {
            engine,
            ..RuntimeConfig::default()
        },
        CapabilityRegistry::new(),
    )
    .expect("runtime");
    let art = rt.load(MODULE.as_bytes()).expect("load");
    let limits = Limits::default().with_max_memory(1024 * 1024);

    let before = rss_bytes();
    if pool {
        pool_mode(&rt, &art, limits, n, pooling, before).await;
        return;
    }
    let mut isolates = Vec::with_capacity(n);
    let mut lat = Vec::with_capacity(n);
    for _ in 0..n {
        let t = Instant::now();
        let mut iso = rt
            .spawn_isolate(&art, "density", &Grants::none(), limits.clone())
            .await
            .expect("spawn");
        iso.init().await.expect("init");
        lat.push(t.elapsed());
        isolates.push(iso);
    }
    let after = rss_bytes();
    lat.sort();

    println!(
        "isolates: {n}  allocator: {}",
        if pooling { "pooling" } else { "on-demand" }
    );
    match (before, after) {
        (Some(b), Some(a)) => {
            let delta = a.saturating_sub(b);
            println!("rss before: {b} B, after: {a} B, delta: {delta} B");
            println!("bytes per isolate: {}", delta / n as u64);
        }
        _ => println!("memory: unsupported on this platform"),
    }
    println!(
        "spawn+init latency: p50 {:?}  p99 {:?}  max {:?}",
        percentile(&lat, 0.50),
        percentile(&lat, 0.99),
        lat[lat.len() - 1]
    );
    for mut iso in isolates {
        iso.terminate();
    }
}

/// N idle isolates in an instance pool: memory per idle pooled isolate, acquire latency,
/// and reclaiming under memory pressure.
async fn pool_mode(
    rt: &Runtime,
    art: &std::sync::Arc<orbit_runtime::Artifact>,
    limits: Limits,
    n: usize,
    pooling: bool,
    before: Option<u64>,
) {
    let cfg = PoolConfig::default()
        .with_max_idle(n)
        .with_max_total(n)
        .with_idle_timeout(None);
    let pool = rt
        .pool(art, "density", &Grants::none(), limits, cfg)
        .expect("pool");
    let t = Instant::now();
    pool.prewarm(n).await.expect("prewarm");
    let prewarm = t.elapsed();
    let after = rss_bytes();
    let s = pool.stats();
    println!(
        "pool of {} idle isolates  allocator: {}",
        s.idle,
        if pooling { "pooling" } else { "on-demand" }
    );
    println!(
        "prewarm: {prewarm:?} total, {:?} per isolate; admission reserved: {} B ({} B per isolate)",
        prewarm / n as u32,
        s.idle_reserved_bytes,
        s.idle_reserved_bytes / n as u64
    );
    if let (Some(b), Some(a)) = (before, after) {
        let delta = a.saturating_sub(b);
        println!(
            "rss delta: {delta} B, per idle isolate: {} B",
            delta / n as u64
        );
    } else {
        println!("memory: unsupported on this platform");
    }

    // Acquire (hit) + release, one isolate at a time: fresh, so each one is terminated and
    // the pool shrinks; the next acquires are hits until the prewarmed ones run out.
    let k = n.min(1000);
    let mut lat = Vec::with_capacity(k);
    for _ in 0..k {
        let t = Instant::now();
        let iso = pool.acquire().await.expect("acquire");
        lat.push(t.elapsed());
        drop(iso);
    }
    lat.sort();
    let s = pool.stats();
    println!(
        "acquire (fresh, prewarmed) x{k}: p50 {:?}  p99 {:?}  max {:?}  (hits {}, misses {})",
        percentile(&lat, 0.50),
        percentile(&lat, 0.99),
        lat[lat.len() - 1],
        s.hits,
        s.misses
    );

    pool.prewarm(n).await.expect("prewarm");
    let rss_full = rss_bytes();
    let freed = rt.reclaim_idle(u64::MAX);
    let rss_empty = rss_bytes();
    println!(
        "reclaim_idle: freed {freed} B of admission budget; reserved now {} B; rss {:?} -> {:?}",
        rt.reserved_memory_bytes(),
        rss_full,
        rss_empty
    );
}
