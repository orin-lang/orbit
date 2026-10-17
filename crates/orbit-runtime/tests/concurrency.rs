mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::*;
use orbit_runtime::{Grants, IsolateState, Limits, RuntimeConfig};

const WORK: &str = r#"
(module
  (memory 1)
  (func (export "work") (param i32) (result i32) (local $i i32) (local $acc i32)
    (loop $l
      (local.set $acc (i32.add (local.get $acc) (local.get $i)))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $l (i32.lt_u (local.get $i) (local.get 0))))
    (local.get $acc)))
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_hundred_isolates_share_a_small_slot_pool() {
    let (rt, _) = runtime_metered_with(RuntimeConfig {
        max_concurrent_invocations: 4,
        memory_budget_bytes: 200 * 1024 * 1024,
        ..RuntimeConfig::default()
    });
    let art = load(&rt, WORK);
    let running = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));

    let mut tasks = Vec::new();
    for n in 0..200u32 {
        let rt = rt.clone();
        let art = Arc::clone(&art);
        let (running, peak) = (Arc::clone(&running), Arc::clone(&peak));
        tasks.push(tokio::spawn(async move {
            let limits = Limits::default()
                .with_max_memory(1024 * 1024)
                .with_fuel(50_000_000);
            let mut iso = rt
                .spawn_isolate(&art, format!("tenant-{}", n % 7), &Grants::none(), limits)
                .await
                .unwrap();
            iso.init().await.unwrap();
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            let out = iso.invoke("work", &[10_000 + i64::from(n)]).await.unwrap();
            running.fetch_sub(1, Ordering::SeqCst);
            let k = 10_000 + i64::from(n);
            assert_eq!(out, vec![k * (k - 1) / 2]);
            assert_eq!(iso.state(), IsolateState::Idle);
            iso.terminate();
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    assert_eq!(rt.reserved_memory_bytes(), 0);
    assert_eq!(rt.available_invocation_slots(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slots_bound_concurrently_executing_guests() {
    // With 2 slots and a guest that spins until its deadline, at most 2 can be running at
    // once; the others queue and still finish.
    let (rt, _) = runtime_with(RuntimeConfig {
        max_concurrent_invocations: 2,
        ..RuntimeConfig::default()
    });
    let art = load(&rt, SPIN);
    let start = std::time::Instant::now();
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let (rt, art) = (rt.clone(), Arc::clone(&art));
        tasks.push(tokio::spawn(async move {
            let limits = Limits::default().with_timeout(std::time::Duration::from_millis(100));
            let mut iso = rt
                .spawn_isolate(&art, "t", &Grants::none(), limits)
                .await
                .unwrap();
            iso.init().await.unwrap();
            iso.invoke("spin", &[]).await.unwrap_err()
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), orbit_runtime::ExecError::DeadlineExceeded);
    }
    // 6 guests x 100 ms over 2 slots needs at least 3 rounds.
    assert!(
        start.elapsed() >= std::time::Duration::from_millis(290),
        "{:?}",
        start.elapsed()
    );
}
