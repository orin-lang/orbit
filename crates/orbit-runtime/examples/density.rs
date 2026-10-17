//! Isolate density: spawn N idle, initialised isolates and report memory and spawn latency.
//!
//! Usage: `cargo run --release -p orbit-runtime --example density -- [N] [--pooling]`
//!
//! Memory is the process resident set (`VmRSS` in `/proc/self/status`) before and after, so
//! it includes everything the process allocated while spawning, not only guest memory.

use std::time::{Duration, Instant};

use orbit_runtime::{
    CapabilityRegistry, EngineConfig, Grants, Limits, PoolingConfig, Runtime, RuntimeConfig,
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
    for arg in std::env::args().skip(1) {
        if arg == "--pooling" {
            pooling = true;
        } else if let Ok(v) = arg.parse() {
            n = v;
        } else {
            eprintln!("usage: density [N] [--pooling]");
            std::process::exit(2);
        }
    }
    let mut engine = EngineConfig::default();
    if pooling {
        engine.pooling = Some(PoolingConfig {
            max_instances: u32::try_from(n).expect("N fits u32"),
            max_memory_bytes: 1024 * 1024,
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
