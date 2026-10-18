//! `orbit:net/v1`: a WAT guest with raw pass-throughs to every import plus an in-guest echo
//! loop, driven by a Rust TCP client.

use std::time::{Duration, Instant};

use orbit_abi::{CapabilityId, status};
use orbit_caps::{NetCapability, NetController, net_grant_config};
use orbit_runtime::{
    CapabilityRegistry, Grants, InstantiateError, Isolate, Limits, Runtime, RuntimeConfig,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Data: "127.0.0.1" at 0, "127.0.0.2" at 16, "localhost" at 32, "example.org" at 48.
/// Scratch buffer at 4096 (`poke` / `peek`). Memory is 2 pages.
fn net_wat(is64: bool) -> String {
    let (mem, off, peek, poke) = if is64 {
        (
            "(memory (export \"memory\") i64 2)",
            "(i64.const 0)",
            "(i32.load8_u (local.get 0))",
            "(i32.store8 (local.get 0) (local.get 1))",
        )
    } else {
        (
            "(memory (export \"memory\") 2)",
            "(i32.const 0)",
            "(i32.load8_u (i32.wrap_i64 (local.get 0)))",
            "(i32.store8 (i32.wrap_i64 (local.get 0)) (local.get 1))",
        )
    };
    format!(
        r#"(module
  (import "orbit:net/v1" "tcp_listen" (func $listen (param i64 i64 i32 i32 i32) (result i64)))
  (import "orbit:net/v1" "local_port" (func $local_port (param i64) (result i32)))
  (import "orbit:net/v1" "tcp_accept" (func $accept (param i64 i32 i64) (result i64)))
  (import "orbit:net/v1" "peer" (func $peer (param i64 i64 i64) (result i64)))
  (import "orbit:net/v1" "peer_port" (func $peer_port (param i64) (result i32)))
  (import "orbit:net/v1" "tcp_connect" (func $connect (param i64 i64 i32 i32 i64) (result i64)))
  (import "orbit:net/v1" "recv" (func $recv (param i64 i64 i64 i32 i64) (result i64)))
  (import "orbit:net/v1" "send" (func $send (param i64 i64 i64 i32 i64) (result i64)))
  (import "orbit:net/v1" "shutdown" (func $shutdown (param i64 i32) (result i32)))
  (import "orbit:net/v1" "close" (func $close (param i64) (result i32)))
  (import "orbit:net/v1" "dns_resolve" (func $dns (param i64 i64 i64 i64) (result i64)))
  (import "orbit:net/v1" "wake_open" (func $wake_open (param i64) (result i64)))
  (import "orbit:net/v1" "wake_signal" (func $wake_signal (param i64) (result i32)))
  (import "orbit:net/v1" "wake_check" (func $wake_check (param i64) (result i32)))
  (import "orbit:net/v1" "wake_close" (func $wake_close (param i64) (result i32)))
  {mem}
  (data {off} "127.0.0.1\00\00\00\00\00\00\00127.0.0.2\00\00\00\00\00\00\00localhost\00\00\00\00\00\00\00example.org")
  (func (export "tcp_listen") (param i64 i64 i32 i32 i32) (result i64)
    (call $listen (local.get 0) (local.get 1) (local.get 2) (local.get 3) (local.get 4)))
  (func (export "local_port") (param i64) (result i32) (call $local_port (local.get 0)))
  (func (export "tcp_accept") (param i64 i32 i64) (result i64)
    (call $accept (local.get 0) (local.get 1) (local.get 2)))
  (func (export "peer") (param i64 i64 i64) (result i64)
    (call $peer (local.get 0) (local.get 1) (local.get 2)))
  (func (export "peer_port") (param i64) (result i32) (call $peer_port (local.get 0)))
  (func (export "tcp_connect") (param i64 i64 i32 i32 i64) (result i64)
    (call $connect (local.get 0) (local.get 1) (local.get 2) (local.get 3) (local.get 4)))
  (func (export "recv") (param i64 i64 i64 i32 i64) (result i64)
    (call $recv (local.get 0) (local.get 1) (local.get 2) (local.get 3) (local.get 4)))
  (func (export "send") (param i64 i64 i64 i32 i64) (result i64)
    (call $send (local.get 0) (local.get 1) (local.get 2) (local.get 3) (local.get 4)))
  (func (export "shutdown") (param i64 i32) (result i32) (call $shutdown (local.get 0) (local.get 1)))
  (func (export "close") (param i64) (result i32) (call $close (local.get 0)))
  (func (export "dns_resolve") (param i64 i64 i64 i64) (result i64)
    (call $dns (local.get 0) (local.get 1) (local.get 2) (local.get 3)))
  (func (export "wake_open") (param i64) (result i64) (call $wake_open (local.get 0)))
  (func (export "wake_signal") (param i64) (result i32) (call $wake_signal (local.get 0)))
  (func (export "wake_check") (param i64) (result i32) (call $wake_check (local.get 0)))
  (func (export "wake_close") (param i64) (result i32) (call $wake_close (local.get 0)))
  (func (export "peek") (param i64) (result i32) {peek})
  (func (export "poke") (param i64 i32) {poke})
  ;; Accepts one connection, echoes everything until end of stream, closes it, and returns
  ;; the number of bytes echoed (or a negative status).
  (func (export "echo_once") (param $l i64) (result i64)
    (local $s i64) (local $n i64) (local $off i64) (local $r i64) (local $tot i64)
    (local.set $s (call $accept (local.get $l) (i32.const 5000) (i64.const 0)))
    (if (i64.lt_s (local.get $s) (i64.const 0)) (then (return (local.get $s))))
    (block $done
      (loop $next
        (local.set $n (call $recv (local.get $s) (i64.const 4096) (i64.const 1024) (i32.const 5000) (i64.const 0)))
        (br_if $done (i64.le_s (local.get $n) (i64.const 0)))
        (local.set $tot (i64.add (local.get $tot) (local.get $n)))
        (local.set $off (i64.const 0))
        (loop $out
          (local.set $r (call $send (local.get $s)
            (i64.add (i64.const 4096) (local.get $off)) (i64.sub (local.get $n) (local.get $off))
            (i32.const 5000) (i64.const 0)))
          (if (i64.lt_s (local.get $r) (i64.const 0)) (then (return (local.get $r))))
          (local.set $off (i64.add (local.get $off) (local.get $r)))
          (br_if $out (i64.lt_s (local.get $off) (local.get $n))))
        (br $next)))
    (drop (call $close (local.get $s)))
    (local.get $tot)))"#
    )
}

fn runtime() -> Runtime {
    Runtime::new(
        RuntimeConfig::default(),
        CapabilityRegistry::new().with(NetCapability::new()),
    )
    .unwrap()
}

fn grants(config: serde_json::Value) -> Grants {
    Grants::none().grant_with(CapabilityId::net_v1(), config)
}

fn loopback() -> serde_json::Value {
    net_grant_config(
        &["127.0.0.1:0-65535".into()],
        &["127.0.0.1:1-65535".into(), "localhost:1-65535".into()],
    )
}

async fn spawn(rt: &Runtime, wat: &str, g: &Grants, limits: Limits) -> Isolate {
    let art = rt.load(wat.as_bytes()).expect("loads");
    let mut iso = rt.spawn_isolate(&art, "t", g, limits).await.unwrap();
    iso.init().await.unwrap();
    iso
}

async fn call(iso: &mut Isolate, name: &str, args: &[i64]) -> i64 {
    iso.invoke(name, args)
        .await
        .unwrap()
        .first()
        .copied()
        .unwrap_or(0)
}

/// Listens on 127.0.0.1:0; returns (listener handle, port).
async fn listen(iso: &mut Isolate) -> (i64, u16) {
    let l = call(iso, "tcp_listen", &[0, 9, 0, 16, 1]).await;
    assert!(l > 0, "listen failed: {l}");
    let port = call(iso, "local_port", &[l]).await;
    (l, u16::try_from(port).unwrap())
}

const E: fn(i32) -> i64 = |c| c as i64;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn echo_roundtrip_wasm32_and_wasm64() {
    for is64 in [false, true] {
        let rt = runtime();
        let mut iso = spawn(&rt, &net_wat(is64), &grants(loopback()), Limits::default()).await;
        let (l, port) = listen(&mut iso).await;
        let server = tokio::spawn(async move {
            let n = call(&mut iso, "echo_once", &[l]).await;
            (iso, n)
        });
        let mut c = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let msg: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
        c.write_all(&msg).await.unwrap();
        c.shutdown().await.unwrap();
        let mut back = Vec::new();
        c.read_to_end(&mut back).await.unwrap();
        assert_eq!(back, msg, "is64={is64}");
        let (mut iso, n) = server.await.unwrap();
        assert_eq!(n, 3000);
        // Only the listener is left open.
        assert_eq!(NetController::of(&mut iso).unwrap().open_sockets(), 1);
        iso.terminate();
    }
}

#[tokio::test]
async fn guest_connects_and_reads_peer() {
    let rt = runtime();
    let mut iso = spawn(&rt, &net_wat(false), &grants(loopback()), Limits::default()).await;
    let srv = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = srv.local_addr().unwrap().port();
    let accept = tokio::spawn(async move {
        let (mut s, _) = srv.accept().await.unwrap();
        s.write_all(b"hi").await.unwrap();
        s.shutdown().await.unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.unwrap();
        buf
    });
    let h = call(&mut iso, "tcp_connect", &[0, 9, i64::from(port), 2000, 0]).await;
    assert!(h > 0, "{h}");
    assert_eq!(call(&mut iso, "peer_port", &[h]).await, i64::from(port));
    assert_eq!(call(&mut iso, "peer", &[h, 8192, 64]).await, 9);
    assert_eq!(
        call(&mut iso, "peer", &[h, 8192, 3]).await,
        E(status::ERR_TOO_LARGE)
    );
    assert_eq!(call(&mut iso, "recv", &[h, 4096, 100, 2000, 0]).await, 2);
    assert_eq!(call(&mut iso, "peek", &[4096]).await, i64::from(b'h'));
    // Orderly end of stream.
    assert_eq!(
        call(&mut iso, "recv", &[h, 4096, 100, 2000, 0]).await,
        E(status::ERR_EOF)
    );
    call(&mut iso, "poke", &[4096, 7]).await;
    assert_eq!(call(&mut iso, "send", &[h, 4096, 1, 2000, 0]).await, 1);
    assert_eq!(call(&mut iso, "shutdown", &[h, 1]).await, 0);
    assert_eq!(
        call(&mut iso, "shutdown", &[h, 9]).await,
        E(status::ERR_INVALID)
    );
    assert_eq!(call(&mut iso, "close", &[h]).await, 0);
    assert_eq!(accept.await.unwrap(), vec![7]);
}

#[tokio::test]
async fn denied_without_scopes_and_outside_scopes() {
    let rt = runtime();
    // Granted capability, no scopes: everything is ERR_DENIED, no socket is created.
    let mut iso = spawn(
        &rt,
        &net_wat(false),
        &grants(serde_json::json!({})),
        Limits::default(),
    )
    .await;
    assert_eq!(
        call(&mut iso, "tcp_listen", &[0, 9, 0, 16, 0]).await,
        E(status::ERR_DENIED)
    );
    assert_eq!(
        call(&mut iso, "tcp_connect", &[0, 9, 80, 100, 0]).await,
        E(status::ERR_DENIED)
    );
    assert_eq!(
        call(&mut iso, "dns_resolve", &[32, 9, 8192, 100]).await,
        E(status::ERR_DENIED)
    );
    assert_eq!(NetController::of(&mut iso).unwrap().open_sockets(), 0);

    // Scope mismatch: other address, other port, other host.
    let cfg = net_grant_config(
        &["127.0.0.1:40000-40001".into()],
        &["127.0.0.1:9".into(), "*.example.org:443".into()],
    );
    let mut iso = spawn(&rt, &net_wat(false), &grants(cfg), Limits::default()).await;
    assert_eq!(
        call(&mut iso, "tcp_listen", &[16, 9, 40000, 16, 0]).await,
        E(status::ERR_DENIED)
    ); // 127.0.0.2
    assert_eq!(
        call(&mut iso, "tcp_listen", &[0, 9, 40002, 16, 0]).await,
        E(status::ERR_DENIED)
    ); // port
    assert_eq!(
        call(&mut iso, "tcp_connect", &[0, 9, 10, 100, 0]).await,
        E(status::ERR_DENIED)
    );
    assert_eq!(
        call(&mut iso, "tcp_connect", &[32, 9, 9, 100, 0]).await,
        E(status::ERR_DENIED)
    ); // localhost
    assert_eq!(
        call(&mut iso, "tcp_connect", &[48, 11, 443, 100, 0]).await,
        E(status::ERR_DENIED)
    ); // example.org itself
    // In scope but refused by the OS (nothing listens on 9): refused (or a timeout), not a denial.
    let r = call(&mut iso, "tcp_connect", &[0, 9, 9, 1000, 0]).await;
    assert!(
        r == E(status::ERR_REFUSED) || r == E(status::ERR_TIMEOUT),
        "{r}"
    );
}

#[tokio::test]
async fn ungranted_module_does_not_instantiate() {
    let rt = runtime();
    let art = rt.load(net_wat(false).as_bytes()).unwrap();
    let e = rt
        .spawn_isolate(&art, "t", &Grants::none(), Limits::default())
        .await
        .unwrap_err();
    assert!(matches!(e, InstantiateError::NotGranted(id) if id == CapabilityId::net_v1()));
}

#[tokio::test]
async fn invalid_arguments() {
    let rt = runtime();
    let mut iso = spawn(&rt, &net_wat(false), &grants(loopback()), Limits::default()).await;
    // Port out of range, bad backlog, name with `*`, bad timeout.
    assert_eq!(
        call(&mut iso, "tcp_listen", &[0, 9, 70000, 16, 0]).await,
        E(status::ERR_INVALID)
    );
    assert_eq!(
        call(&mut iso, "tcp_listen", &[0, 9, 0, 0, 0]).await,
        E(status::ERR_INVALID)
    );
    assert_eq!(
        call(&mut iso, "tcp_listen", &[0, 9, -1, 16, 0]).await,
        E(status::ERR_INVALID)
    );
    assert_eq!(
        call(&mut iso, "tcp_listen", &[0, 1 << 40, 0, 16, 0]).await,
        E(status::ERR_TOO_LARGE)
    );
    assert_eq!(
        call(&mut iso, "tcp_listen", &[1 << 40, 4, 0, 16, 0]).await,
        E(status::ERR_INVALID)
    );
    let (l, _) = listen(&mut iso).await;
    assert_eq!(
        call(&mut iso, "tcp_accept", &[l, -1, 0]).await,
        E(status::ERR_INVALID)
    );
    assert_eq!(
        call(&mut iso, "tcp_accept", &[l, 3_600_001, 0]).await,
        E(status::ERR_INVALID)
    );
}

#[tokio::test]
async fn accept_timeout_and_deadline_clamp() {
    let rt = runtime();
    let mut iso = spawn(&rt, &net_wat(false), &grants(loopback()), Limits::default()).await;
    let (l, _) = listen(&mut iso).await;
    let t = Instant::now();
    assert_eq!(
        call(&mut iso, "tcp_accept", &[l, 60, 0]).await,
        E(status::ERR_TIMEOUT)
    );
    let e = t.elapsed();
    assert!(
        e >= Duration::from_millis(50) && e < Duration::from_secs(2),
        "{e:?}"
    );
    assert_eq!(
        call(&mut iso, "tcp_accept", &[l, 0, 0]).await,
        E(status::ERR_TIMEOUT)
    );

    // A wait never outlives the isolate's wall-clock deadline.
    let mut iso = spawn(
        &rt,
        &net_wat(false),
        &grants(loopback()),
        Limits::default().with_timeout(Duration::from_millis(150)),
    )
    .await;
    let (l, _) = listen(&mut iso).await;
    let t = Instant::now();
    assert_eq!(
        call(&mut iso, "tcp_accept", &[l, 60_000, 0]).await,
        E(status::ERR_TIMEOUT)
    );
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
}

#[tokio::test]
async fn recv_timeout_on_silent_peer() {
    let rt = runtime();
    let mut iso = spawn(&rt, &net_wat(false), &grants(loopback()), Limits::default()).await;
    let (l, port) = listen(&mut iso).await;
    let _client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let s = call(&mut iso, "tcp_accept", &[l, 2000, 0]).await;
    assert!(s > 0);
    assert_eq!(
        call(&mut iso, "recv", &[s, 4096, 10, 50, 0]).await,
        E(status::ERR_TIMEOUT)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wake_signal_cancels_a_wait_in_another_call() {
    let rt = runtime();
    let mut iso = spawn(&rt, &net_wat(false), &grants(loopback()), Limits::default()).await;
    let (l, _) = listen(&mut iso).await;
    let root = call(&mut iso, "wake_open", &[0]).await;
    let child = call(&mut iso, "wake_open", &[root]).await;
    assert!(root > 0 && child > 0 && root != child);
    assert_eq!(call(&mut iso, "wake_check", &[child]).await, 0);
    let ctl = NetController::of(&mut iso).unwrap();

    // Signalling the PARENT interrupts a wait bound to the CHILD, long before the timeout.
    let t = Instant::now();
    let waiter = tokio::spawn(async move {
        let r = call(&mut iso, "tcp_accept", &[l, 60_000, child]).await;
        (iso, r)
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(ctl.signal(root));
    let (mut iso, r) = waiter.await.unwrap();
    assert_eq!(r, E(status::ERR_CANCELLED));
    assert!(t.elapsed() < Duration::from_secs(5));

    // Cancellation is permanent and checked first: a set source cancels new work at once.
    assert_eq!(call(&mut iso, "wake_check", &[child]).await, 1);
    assert_eq!(call(&mut iso, "wake_check", &[root]).await, 1);
    assert_eq!(
        call(&mut iso, "tcp_accept", &[l, 60_000, child]).await,
        E(status::ERR_CANCELLED)
    );
    assert_eq!(
        call(&mut iso, "tcp_connect", &[0, 9, 9, 1000, root]).await,
        E(status::ERR_CANCELLED)
    );
    // A source opened under a set parent starts set; an unrelated source is unaffected.
    let late = call(&mut iso, "wake_open", &[root]).await;
    assert_eq!(call(&mut iso, "wake_check", &[late]).await, 1);
    let other = call(&mut iso, "wake_open", &[0]).await;
    assert_eq!(call(&mut iso, "wake_check", &[other]).await, 0);
    assert_eq!(call(&mut iso, "wake_signal", &[other]).await, 0);
    assert_eq!(call(&mut iso, "wake_check", &[other]).await, 1);
    // Wake handles: close, stale, forged.
    assert_eq!(call(&mut iso, "wake_close", &[other]).await, 0);
    for f in ["wake_close", "wake_signal", "wake_check"] {
        assert_eq!(
            call(&mut iso, f, &[other]).await,
            E(status::ERR_BAD_HANDLE),
            "{f}"
        );
    }
    assert_eq!(
        call(&mut iso, "tcp_accept", &[l, 10, other]).await,
        E(status::ERR_BAD_HANDLE)
    );
    assert_eq!(
        call(&mut iso, "wake_open", &[i64::MAX]).await,
        E(status::ERR_BAD_HANDLE)
    );
    assert!(!ctl.signal(other));
}

#[tokio::test]
async fn handle_misuse() {
    let rt = runtime();
    let g = grants(loopback());
    let mut a = spawn(&rt, &net_wat(false), &g, Limits::default()).await;
    let mut b = spawn(&rt, &net_wat(false), &g, Limits::default()).await;
    let (la, port) = listen(&mut a).await;
    let (lb, _) = listen(&mut b).await;
    // A connected stream in A.
    let _client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let sa = call(&mut a, "tcp_accept", &[la, 2000, 0]).await;
    assert!(sa > 0);

    let bad = E(status::ERR_BAD_HANDLE);
    // Forged values.
    for h in [0, -1, 1, 12345, i64::MAX, i64::MIN] {
        assert_eq!(call(&mut a, "close", &[h]).await, bad, "{h}");
        assert_eq!(call(&mut a, "recv", &[h, 4096, 10, 10, 0]).await, bad);
        assert_eq!(call(&mut a, "tcp_accept", &[h, 10, 0]).await, bad);
        assert_eq!(call(&mut a, "local_port", &[h]).await, bad);
    }
    // Another instance's handles, listener and stream alike.
    for h in [la, sa] {
        assert_eq!(call(&mut b, "close", &[h]).await, bad);
        assert_eq!(call(&mut b, "local_port", &[h]).await, bad);
    }
    assert_ne!(la, lb);
    // Wrong kind: a listener is not a stream and the other way round.
    assert_eq!(call(&mut a, "recv", &[la, 4096, 10, 10, 0]).await, bad);
    assert_eq!(call(&mut a, "send", &[la, 4096, 1, 10, 0]).await, bad);
    assert_eq!(call(&mut a, "peer_port", &[la]).await, bad);
    assert_eq!(call(&mut a, "tcp_accept", &[sa, 10, 0]).await, bad);
    // Closed handles are stale, also after the slot is reused.
    assert_eq!(call(&mut a, "close", &[sa]).await, 0);
    assert_eq!(call(&mut a, "close", &[sa]).await, bad);
    assert_eq!(call(&mut a, "recv", &[sa, 4096, 10, 10, 0]).await, bad);
    let (l2, _) = listen(&mut a).await;
    assert_ne!(l2, sa);
    assert_eq!(call(&mut a, "recv", &[sa, 4096, 10, 10, 0]).await, bad);
}

#[tokio::test]
async fn socket_quota() {
    let rt = runtime();
    let mut cfg = loopback();
    cfg["max_sockets"] = 2.into();
    let mut iso = spawn(&rt, &net_wat(false), &grants(cfg), Limits::default()).await;
    let (l1, port) = listen(&mut iso).await;
    let (l2, _) = listen(&mut iso).await;
    assert_eq!(
        call(&mut iso, "tcp_listen", &[0, 9, 0, 16, 0]).await,
        E(status::ERR_QUOTA)
    );
    assert_eq!(
        call(&mut iso, "tcp_connect", &[0, 9, i64::from(port), 500, 0]).await,
        E(status::ERR_QUOTA)
    );
    assert_eq!(
        call(&mut iso, "tcp_accept", &[l1, 10, 0]).await,
        E(status::ERR_QUOTA)
    );
    // Freeing a slot makes room again.
    assert_eq!(call(&mut iso, "close", &[l2]).await, 0);
    assert!(call(&mut iso, "tcp_listen", &[0, 9, 0, 16, 0]).await > 0);
}

#[tokio::test]
async fn bad_buffers_and_byte_cap() {
    let rt = runtime();
    let mut cfg = loopback();
    cfg["max_bytes_per_call"] = 100.into();
    let mut iso = spawn(&rt, &net_wat(false), &grants(cfg), Limits::default()).await;
    let (l, port) = listen(&mut iso).await;
    let mut c = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let s = call(&mut iso, "tcp_accept", &[l, 2000, 0]).await;
    c.write_all(&[1u8; 300]).await.unwrap();
    // Bad output buffer: refused before any byte is consumed.
    assert_eq!(
        call(&mut iso, "recv", &[s, 131_000, 100, 1000, 0]).await,
        E(status::ERR_INVALID)
    );
    assert_eq!(
        call(&mut iso, "recv", &[s, -1, 100, 1000, 0]).await,
        E(status::ERR_INVALID)
    );
    assert_eq!(
        call(&mut iso, "recv", &[s, 4096, 0, 1000, 0]).await,
        E(status::ERR_INVALID)
    );
    // The cap limits one call; the rest is still there.
    assert_eq!(call(&mut iso, "recv", &[s, 4096, 1000, 1000, 0]).await, 100);
    assert_eq!(call(&mut iso, "recv", &[s, 4096, 1000, 1000, 0]).await, 100);
    // Send: bad buffer, and a request above the cap is served partially.
    assert_eq!(
        call(&mut iso, "send", &[s, 131_000, 100, 1000, 0]).await,
        E(status::ERR_INVALID)
    );
    assert_eq!(
        call(&mut iso, "send", &[s, 4096, -5, 1000, 0]).await,
        E(status::ERR_INVALID)
    );
    assert_eq!(call(&mut iso, "send", &[s, 4096, 250, 1000, 0]).await, 100);
    assert_eq!(call(&mut iso, "send", &[s, 4096, 0, 1000, 0]).await, 0);
}

#[tokio::test]
async fn peer_reset_and_dns() {
    let rt = runtime();
    let mut iso = spawn(&rt, &net_wat(true), &grants(loopback()), Limits::default()).await;
    let (l, port) = listen(&mut iso).await;
    let c = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let s = call(&mut iso, "tcp_accept", &[l, 2000, 0]).await;
    // Abortive close from the client (RST).
    socket2::SockRef::from(&c)
        .set_linger(Some(Duration::ZERO))
        .unwrap();
    drop(c);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let r = call(&mut iso, "recv", &[s, 4096, 10, 1000, 0]).await;
    assert!(r == E(status::ERR_RESET) || r == E(status::ERR_EOF), "{r}");

    // dns_resolve: output format is one numeric address per line.
    let n = call(&mut iso, "dns_resolve", &[32, 9, 8192, 200]).await;
    assert!(n > 0, "{n}");
    let mut text = String::new();
    for i in 0..n {
        text.push(char::from(
            u8::try_from(call(&mut iso, "peek", &[8192 + i]).await).unwrap(),
        ));
    }
    assert!(
        text.lines().any(|l| l == "127.0.0.1" || l == "::1"),
        "{text:?}"
    );
    assert!(text.ends_with('\n'));
    assert_eq!(
        call(&mut iso, "dns_resolve", &[32, 9, 8192, 0]).await,
        E(status::ERR_INVALID)
    );
    assert_eq!(
        call(&mut iso, "dns_resolve", &[32, 9, 8192, 3]).await,
        E(status::ERR_TOO_LARGE)
    );
}

#[tokio::test]
async fn refused_connect_and_address_in_use() {
    let rt = runtime();
    let mut iso = spawn(&rt, &net_wat(false), &grants(loopback()), Limits::default()).await;
    let (_l, port) = listen(&mut iso).await;
    // A second listener on the same port (opt 0, no SO_REUSEADDR): EADDRINUSE.
    assert_eq!(
        call(&mut iso, "tcp_listen", &[0, 9, i64::from(port), 16, 0]).await,
        E(status::ERR_ADDR_IN_USE)
    );
    // A port nobody listens on: ECONNREFUSED (Windows retries for about 2 s first).
    let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let closed = free.local_addr().unwrap().port();
    drop(free);
    assert_eq!(
        call(
            &mut iso,
            "tcp_connect",
            &[0, 9, i64::from(closed), 10_000, 0]
        )
        .await,
        E(status::ERR_REFUSED)
    );
    iso.terminate();
}
