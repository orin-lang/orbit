//! Instance pools: fresh-by-default reuse policy, refill, idle timeout, reclaim, concurrency.

mod common;

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::*;
use orbit_runtime::{
    ExecError, Grants, InstanceId, InstantiateError, IsolateState, Limits, PoolConfig, PoolError,
    ReusePolicy, RuntimeConfig,
};

const MIB: u64 = 1024 * 1024;

fn limits() -> Limits {
    Limits::default().with_max_memory(MIB)
}

const TRAPPY: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "ok") (result i32) (i32.const 7))
  (func (export "boom") unreachable))
"#;

const BAD_INIT: &str = r#"
(module
  (memory 1)
  (func (export "orbit_init") (result i32) (i32.const 1)))
"#;

/// Polls `cond` (every 10 ms of Tokio time, so it works with a paused clock too).
async fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..3000 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

#[tokio::test]
async fn fresh_per_acquire_never_reuses_and_never_leaks_state() {
    let (rt, _) = runtime();
    let art = load(&rt, COUNTER);
    let pool = rt
        .pool(&art, "t", &Grants::none(), limits(), PoolConfig::default())
        .unwrap();
    assert_eq!(pool.config().reuse, ReusePolicy::FreshPerAcquire);
    pool.prewarm(3).await.unwrap();
    assert_eq!(pool.stats().idle, 3);

    let mut ids: HashSet<InstanceId> = HashSet::new();
    for _ in 0..12 {
        let mut iso = pool.acquire().await.unwrap();
        assert_eq!(iso.uses(), 1);
        assert!(ids.insert(iso.id()), "an instance was handed out twice");
        // The per-request global and memory cell always start from zero.
        assert_eq!(iso.invoke("bump", &[]).await.unwrap(), vec![1]);
        assert_eq!(iso.invoke("mem", &[]).await.unwrap(), vec![10]);
    }
    let s = pool.stats();
    assert_eq!(s.hits + s.misses, 12);
    assert!(s.hits >= 3, "{s:?}");
    assert_eq!(s.discarded, 12);
    assert_eq!(s.recycled, 0);
    assert_eq!(s.in_use, 0);
}

#[tokio::test]
async fn reuse_keeps_state_and_honours_max_uses() {
    let (rt, _) = runtime();
    let art = load(&rt, COUNTER);
    let cfg = PoolConfig::default().with_reuse(ReusePolicy::Reuse { max_uses: 3 });
    let pool = rt.pool(&art, "t", &Grants::none(), limits(), cfg).unwrap();

    let mut first = None;
    for n in 1..=3 {
        let mut iso = pool.acquire().await.unwrap();
        assert_eq!(iso.uses(), n);
        let id = *first.get_or_insert(iso.id());
        assert_eq!(iso.id(), id, "same instance for its first three uses");
        assert_eq!(iso.invoke("bump", &[]).await.unwrap(), vec![n as i64]);
    }
    // The third use was the last: the instance is gone, the next one starts from scratch.
    assert_eq!(pool.stats().idle, 0);
    let mut iso = pool.acquire().await.unwrap();
    assert_ne!(Some(iso.id()), first);
    assert_eq!(iso.uses(), 1);
    assert_eq!(iso.invoke("bump", &[]).await.unwrap(), vec![1]);
    drop(iso);

    let s = pool.stats();
    assert_eq!(s.created, 2);
    assert_eq!(s.recycled, 3); // 2 + 1 returns
    assert_eq!(s.discarded, 1);
    assert_eq!(s.idle, 1);
}

#[tokio::test]
async fn max_uses_zero_counts_as_one() {
    let (rt, _) = runtime();
    let art = load(&rt, COUNTER);
    let cfg = PoolConfig::default().with_reuse(ReusePolicy::Reuse { max_uses: 0 });
    let pool = rt.pool(&art, "t", &Grants::none(), limits(), cfg).unwrap();
    drop(pool.acquire().await.unwrap());
    assert_eq!(pool.stats().idle, 0);
}

#[tokio::test]
async fn discard_terminates_even_under_reuse() {
    let (rt, _) = runtime();
    let art = load(&rt, COUNTER);
    let cfg = PoolConfig::default().with_reuse(ReusePolicy::Reuse { max_uses: 100 });
    let pool = rt.pool(&art, "t", &Grants::none(), limits(), cfg).unwrap();
    let iso = pool.acquire().await.unwrap();
    iso.discard();
    let s = pool.stats();
    assert_eq!((s.idle, s.discarded, s.recycled), (0, 1, 0));
    assert_eq!(rt.reserved_memory_bytes(), 0);
}

#[tokio::test]
async fn failed_instances_are_never_returned() {
    let (rt, _) = runtime();
    let art = load(&rt, TRAPPY);
    let cfg = PoolConfig::default().with_reuse(ReusePolicy::Reuse { max_uses: 100 });
    let pool = rt.pool(&art, "t", &Grants::none(), limits(), cfg).unwrap();

    let mut iso = pool.acquire().await.unwrap();
    let bad = iso.id();
    assert!(matches!(
        iso.invoke("boom", &[]).await,
        Err(ExecError::Trap(_))
    ));
    assert_eq!(iso.state(), IsolateState::Failed);
    drop(iso);
    let s = pool.stats();
    assert_eq!((s.idle, s.recycled, s.discarded), (0, 0, 1));
    assert_eq!(rt.reserved_memory_bytes(), 0);

    let mut next = pool.acquire().await.unwrap();
    assert_ne!(next.id(), bad);
    assert_eq!(next.invoke("ok", &[]).await.unwrap(), vec![7]);
    drop(next);
    assert_eq!(pool.stats().idle, 1);
}

#[tokio::test]
async fn a_cancelled_invocation_does_not_poison_the_pool() {
    let (rt, _) = runtime();
    let art = load(&rt, SPIN);
    let cfg = PoolConfig::default().with_reuse(ReusePolicy::Reuse { max_uses: 100 });
    let pool = rt.pool(&art, "t", &Grants::none(), limits(), cfg).unwrap();
    let mut iso = pool.acquire().await.unwrap();
    let id = iso.id();
    // The caller gives up while the guest is running: the isolate is left `Running`.
    let r = tokio::time::timeout(Duration::from_millis(100), iso.invoke("spin", &[])).await;
    assert!(r.is_err());
    assert_eq!(iso.state(), IsolateState::Running);
    drop(iso);
    assert_eq!(pool.stats().idle, 0);
    let again = pool.acquire().await.unwrap();
    assert_ne!(again.id(), id);
}

#[tokio::test]
async fn prewarm_and_min_idle_refill() {
    let (rt, _) = runtime();
    let art = load(&rt, COUNTER);

    // prewarm is bounded by max_idle.
    let pool = rt
        .pool(
            &art,
            "t",
            &Grants::none(),
            limits(),
            PoolConfig::default().with_max_idle(4).with_max_total(8),
        )
        .unwrap();
    assert_eq!(pool.prewarm(10).await.unwrap(), 4);
    assert_eq!(pool.prewarm(10).await.unwrap(), 0);
    let s = pool.stats();
    assert_eq!((s.idle, s.created), (4, 4));
    assert_eq!(s.idle_reserved_bytes, 4 * MIB);
    assert_eq!(rt.reserved_memory_bytes(), 4 * MIB);
    drop(pool);
    assert_eq!(rt.reserved_memory_bytes(), 0);

    // min_idle: filled in the background at creation and refilled after an acquire.
    let pool = rt
        .pool(
            &art,
            "t",
            &Grants::none(),
            limits(),
            PoolConfig::default().with_min_idle(3).with_max_total(8),
        )
        .unwrap();
    eventually("initial fill", || pool.stats().idle == 3).await;
    let guard = pool.acquire().await.unwrap();
    assert_eq!(pool.stats().hits, 1);
    eventually("refill while one is checked out", || pool.stats().idle == 3).await;
    assert_eq!(pool.stats().in_use, 1);
    drop(guard); // fresh: terminated, idle stays at min_idle
    eventually("settle", || pool.stats().in_use == 0).await;
    assert_eq!(pool.stats().idle, 3);
    assert_eq!(rt.reserved_memory_bytes(), 3 * MIB);
}

#[tokio::test(start_paused = true)]
async fn idle_timeout_scales_to_zero() {
    let (rt, _) = runtime();
    let art = load(&rt, COUNTER);
    let cfg = PoolConfig::default().with_idle_timeout(Some(Duration::from_secs(30)));
    let pool = rt.pool(&art, "t", &Grants::none(), limits(), cfg).unwrap();
    pool.prewarm(3).await.unwrap();
    assert_eq!(rt.reserved_memory_bytes(), 3 * MIB);

    tokio::time::advance(Duration::from_secs(10)).await;
    tokio::task::yield_now().await;
    assert_eq!(pool.stats().idle, 3, "not old enough yet");

    tokio::time::advance(Duration::from_secs(25)).await;
    eventually("eviction", || pool.stats().idle == 0).await;
    let s = pool.stats();
    assert_eq!(s.evicted, 3);
    assert_eq!(rt.reserved_memory_bytes(), 0);

    // And it comes back on demand.
    let mut iso = pool.acquire().await.unwrap();
    assert_eq!(iso.invoke("bump", &[]).await.unwrap(), vec![1]);
}

#[tokio::test(start_paused = true)]
async fn idle_timeout_stops_at_min_idle() {
    let (rt, _) = runtime();
    let art = load(&rt, COUNTER);
    let cfg = PoolConfig::default()
        .with_min_idle(1)
        .with_idle_timeout(Some(Duration::from_secs(30)));
    let pool = rt.pool(&art, "t", &Grants::none(), limits(), cfg).unwrap();
    pool.prewarm(3).await.unwrap();
    tokio::time::advance(Duration::from_secs(100)).await;
    eventually("eviction down to min_idle", || pool.stats().idle == 1).await;
    tokio::time::advance(Duration::from_secs(1000)).await;
    tokio::task::yield_now().await;
    assert_eq!(pool.stats().idle, 1);
    assert_eq!(pool.stats().evicted, 2);
}

#[tokio::test]
async fn reclaim_idle_frees_reservation_for_a_failing_spawn() {
    // Budget for exactly three isolates; a pool holds all three idle.
    let (rt, _) = runtime_with(RuntimeConfig {
        memory_budget_bytes: 3 * MIB,
        ..RuntimeConfig::default()
    });
    let art = load(&rt, COUNTER);
    let pool = rt
        .pool(&art, "t", &Grants::none(), limits(), PoolConfig::default())
        .unwrap();
    pool.prewarm(3).await.unwrap();
    assert_eq!(rt.reserved_memory_bytes(), 3 * MIB);

    // spawn_isolate evicts exactly one idle isolate to make room.
    let mut other = rt
        .spawn_isolate(&art, "other", &Grants::none(), limits())
        .await
        .unwrap();
    other.init().await.unwrap();
    assert_eq!(pool.stats().idle, 2);
    assert_eq!(pool.stats().evicted, 1);
    assert_eq!(rt.reserved_memory_bytes(), 3 * MIB);

    // Explicit reclaim; checked-out isolates are never touched.
    let held = pool.acquire().await.unwrap();
    let freed = rt.reclaim_idle(u64::MAX);
    assert_eq!(freed, MIB);
    assert_eq!(pool.stats().idle, 0);
    assert_eq!(rt.reserved_memory_bytes(), 2 * MIB);
    assert_eq!(held.state(), IsolateState::Initialized);
    // Nothing idle left: a request that cannot fit is refused as before.
    let err = rt
        .spawn_isolate(
            &art,
            "big",
            &Grants::none(),
            limits().with_max_memory(2 * MIB),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, InstantiateError::Admission(_)), "{err:?}");
    // reclaim with nothing to do returns 0.
    assert_eq!(rt.reclaim_idle(MIB), 0);
}

#[tokio::test]
async fn without_pools_admission_still_fails_fast() {
    let (rt, _) = runtime_with(RuntimeConfig {
        memory_budget_bytes: MIB,
        ..RuntimeConfig::default()
    });
    let art = load(&rt, COUNTER);
    let _a = rt
        .spawn_isolate(&art, "t", &Grants::none(), limits())
        .await
        .unwrap();
    let err = rt
        .spawn_isolate(&art, "t", &Grants::none(), limits())
        .await
        .unwrap_err();
    assert!(matches!(err, InstantiateError::Admission(_)));
}

#[tokio::test(start_paused = true)]
async fn reclaim_evicts_the_least_recently_used_pool_first() {
    let (rt, _) = runtime_with(RuntimeConfig {
        memory_budget_bytes: 2 * MIB,
        ..RuntimeConfig::default()
    });
    let art = load(&rt, COUNTER);
    let cfg = PoolConfig::default().with_idle_timeout(None);
    let old = rt
        .pool(&art, "old", &Grants::none(), limits(), cfg.clone())
        .unwrap();
    let new = rt
        .pool(&art, "new", &Grants::none(), limits(), cfg)
        .unwrap();
    old.prewarm(1).await.unwrap();
    tokio::time::advance(Duration::from_secs(5)).await;
    new.prewarm(1).await.unwrap();
    assert_eq!(rt.reclaim_idle(MIB), MIB);
    assert_eq!((old.stats().idle, new.stats().idle), (0, 1));
}

#[tokio::test]
async fn pools_of_different_tenants_are_distinct() {
    let (rt, _) = runtime();
    let art = load(&rt, COUNTER);
    let cfg = PoolConfig::default().with_reuse(ReusePolicy::Reuse { max_uses: 100 });
    let a = rt
        .pool(&art, "tenant-a", &Grants::none(), limits(), cfg.clone())
        .unwrap();
    let b = rt
        .pool(&art, "tenant-b", &Grants::none(), limits(), cfg)
        .unwrap();
    assert_eq!(a.tenant().as_str(), "tenant-a");
    assert_eq!(b.tenant().as_str(), "tenant-b");

    let mut ia = a.acquire().await.unwrap();
    assert_eq!(ia.tenant().as_str(), "tenant-a");
    ia.invoke("bump", &[]).await.unwrap();
    ia.invoke("bump", &[]).await.unwrap();
    let id_a = ia.id();
    drop(ia);

    // B never sees A's idle instance or its state.
    let mut ib = b.acquire().await.unwrap();
    assert_eq!(ib.tenant().as_str(), "tenant-b");
    assert_ne!(ib.id(), id_a);
    assert_eq!(ib.invoke("global", &[]).await.unwrap(), vec![0]);
    drop(ib);

    let ia = a.acquire().await.unwrap();
    assert_eq!(ia.id(), id_a);
    assert_eq!(a.stats().hits, 1);
    assert_eq!(b.stats().hits, 0);
}

#[tokio::test]
async fn pool_creation_checks_grants_and_fuel_up_front() {
    let (rt, _) = runtime();
    let art = load(
        &rt,
        r#"(module (import "orbit:log/v1" "write" (func (param i32 i64 i64) (result i32))) (memory (export "memory") 1))"#,
    );
    let err = rt
        .pool(&art, "t", &Grants::none(), limits(), PoolConfig::default())
        .unwrap_err();
    assert!(matches!(err, InstantiateError::NotGranted(_)));
    rt.pool(&art, "t", &log_grant(), limits(), PoolConfig::default())
        .unwrap();

    let err = rt
        .pool(
            &art,
            "t",
            &log_grant(),
            limits().with_fuel(1_000_000),
            PoolConfig::default(),
        )
        .unwrap_err();
    assert!(matches!(err, InstantiateError::FuelMeteringDisabled));
}

#[tokio::test]
async fn failing_initialisation_surfaces_and_releases_the_slot() {
    let (rt, _) = runtime();
    let art = load(&rt, BAD_INIT);
    let pool = rt
        .pool(
            &art,
            "t",
            &Grants::none(),
            limits(),
            PoolConfig::default().with_max_total(1),
        )
        .unwrap();
    for _ in 0..3 {
        let err = pool.acquire().await.unwrap_err();
        assert!(
            matches!(err, PoolError::Init(ExecError::InitFailed(1))),
            "{err:?}"
        );
    }
    let s = pool.stats();
    assert_eq!((s.create_failures, s.in_use, s.idle), (3, 0, 0));
    assert_eq!(rt.reserved_memory_bytes(), 0);
}

#[tokio::test]
async fn admission_failure_during_acquire_is_reported_and_leaves_no_slot_behind() {
    let (rt, _) = runtime_with(RuntimeConfig {
        memory_budget_bytes: MIB / 2,
        ..RuntimeConfig::default()
    });
    let art = load(&rt, COUNTER);
    let pool = rt
        .pool(&art, "t", &Grants::none(), limits(), PoolConfig::default())
        .unwrap();
    let err = pool.acquire().await.unwrap_err();
    assert!(matches!(
        err,
        PoolError::Instantiate(InstantiateError::Admission(_))
    ));
    assert_eq!(pool.stats().in_use, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn try_acquire_and_waiting_acquire_at_max_total() {
    let (rt, _) = runtime();
    let art = load(&rt, COUNTER);
    let cfg = PoolConfig::default()
        .with_max_total(1)
        .with_reuse(ReusePolicy::Reuse { max_uses: 100 });
    let pool = rt.pool(&art, "t", &Grants::none(), limits(), cfg).unwrap();
    let mut first = pool.acquire().await.unwrap();
    first.invoke("bump", &[]).await.unwrap();
    assert!(pool.try_acquire().await.unwrap().is_none());

    // A waiting acquire that times out leaves the pool intact.
    let r = tokio::time::timeout(Duration::from_millis(50), pool.acquire()).await;
    assert!(r.is_err());

    let waiter = {
        let pool = pool.clone();
        tokio::spawn(async move {
            let mut iso = pool.acquire().await.unwrap();
            iso.invoke("bump", &[]).await.unwrap()
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!waiter.is_finished());
    first.release();
    let v = tokio::time::timeout(Duration::from_secs(30), waiter)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(v, vec![2], "the waiter got the recycled instance");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn five_hundred_acquires_across_eight_tasks_respect_max_total() {
    for reuse in [
        ReusePolicy::FreshPerAcquire,
        ReusePolicy::Reuse { max_uses: 25 },
    ] {
        let (rt, _) = runtime();
        let art = load(&rt, COUNTER);
        let cfg = PoolConfig::default()
            .with_max_total(4)
            .with_max_idle(4)
            .with_reuse(reuse);
        let pool = rt.pool(&art, "t", &Grants::none(), limits(), cfg).unwrap();
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let served = Arc::new(AtomicUsize::new(0));
        let fresh_ok = Arc::new(AtomicUsize::new(0));

        let work = async {
            let mut tasks = Vec::new();
            for t in 0..8usize {
                let pool = pool.clone();
                let (running, peak, served, fresh_ok) = (
                    Arc::clone(&running),
                    Arc::clone(&peak),
                    Arc::clone(&served),
                    Arc::clone(&fresh_ok),
                );
                let rt = rt.clone();
                let n = if t < 4 { 63 } else { 62 }; // 4*63 + 4*62 = 500
                tasks.push(tokio::spawn(async move {
                    for _ in 0..n {
                        let mut iso = pool.acquire().await.unwrap();
                        let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        assert!(rt.reserved_memory_bytes() <= 4 * MIB);
                        let v = iso.invoke("bump", &[]).await.unwrap();
                        if v == vec![1] {
                            fresh_ok.fetch_add(1, Ordering::SeqCst);
                        }
                        running.fetch_sub(1, Ordering::SeqCst);
                        served.fetch_add(1, Ordering::SeqCst);
                        if t % 2 == 0 {
                            tokio::task::yield_now().await;
                        }
                    }
                }));
            }
            for t in tasks {
                t.await.unwrap();
            }
        };
        tokio::time::timeout(Duration::from_secs(120), work)
            .await
            .expect("no deadlock");

        assert_eq!(served.load(Ordering::SeqCst), 500);
        assert!(
            peak.load(Ordering::SeqCst) <= 4,
            "{}",
            peak.load(Ordering::SeqCst)
        );
        let s = pool.stats();
        assert_eq!(s.hits + s.misses, 500, "{s:?}");
        assert_eq!(s.in_use, 0);
        assert!(s.idle <= 4);
        match reuse {
            ReusePolicy::FreshPerAcquire => {
                // Every request saw a pristine instance.
                assert_eq!(fresh_ok.load(Ordering::SeqCst), 500);
                assert_eq!(s.discarded, 500);
            }
            ReusePolicy::Reuse { .. } => {
                assert!(s.created < 500 && s.recycled > 0, "{s:?}");
            }
        }
        drop(pool);
        assert_eq!(rt.reserved_memory_bytes(), 0);
    }
}

#[tokio::test]
async fn closing_the_pool_terminates_idle_isolates_and_refuses_acquires() {
    let (rt, _) = runtime();
    let art = load(&rt, COUNTER);
    let cfg = PoolConfig::default().with_reuse(ReusePolicy::Reuse { max_uses: 100 });
    let pool = rt.pool(&art, "t", &Grants::none(), limits(), cfg).unwrap();
    pool.prewarm(2).await.unwrap();
    let out = pool.acquire().await.unwrap();
    assert_eq!(rt.reserved_memory_bytes(), 2 * MIB);

    pool.close();
    assert_eq!(rt.reserved_memory_bytes(), MIB, "only the checked-out one");
    assert!(matches!(pool.acquire().await, Err(PoolError::Closed)));
    drop(out); // not returned to a closed pool
    assert_eq!(rt.reserved_memory_bytes(), 0);
    assert_eq!(pool.stats().idle, 0);
}

#[tokio::test]
async fn dropping_the_last_handle_stops_the_pool() {
    let (rt, _) = runtime();
    let art = load(&rt, COUNTER);
    let pool = rt
        .pool(
            &art,
            "t",
            &Grants::none(),
            limits(),
            PoolConfig::default().with_min_idle(2),
        )
        .unwrap();
    eventually("fill", || pool.stats().idle == 2).await;
    let clone = pool.clone();
    drop(pool);
    assert_eq!(
        rt.reserved_memory_bytes(),
        2 * MIB,
        "a clone keeps it alive"
    );
    drop(clone);
    assert_eq!(rt.reserved_memory_bytes(), 0);
    // The background task does not resurrect anything.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(rt.reserved_memory_bytes(), 0);
}
