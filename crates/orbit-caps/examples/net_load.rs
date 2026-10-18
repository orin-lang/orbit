//! Load test of `orbit:net/v1`: requests per second through an echo guest.
//!
//! `cargo run --release -p orbit-caps --example net_load -- [isolates] [clients_per_isolate] [seconds]`
//!
//! Each isolate runs the same WAT server (accept, recv, send, close, repeat) on its own
//! listener; each client does connect, write 64 bytes, read them back, close. Latency is per
//! request, connect included. Everything is on localhost in one process: the client load
//! shares the machine with the server.

use std::time::{Duration, Instant};

use orbit_abi::CapabilityId;
use orbit_caps::{NetCapability, net_grant_config};
use orbit_runtime::{CapabilityRegistry, Grants, Limits, Runtime, RuntimeConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const SERVER: &str = r#"(module
  (import "orbit:net/v1" "tcp_listen" (func $listen (param i64 i64 i32 i32 i32) (result i64)))
  (import "orbit:net/v1" "local_port" (func $local_port (param i64) (result i32)))
  (import "orbit:net/v1" "tcp_accept" (func $accept (param i64 i32 i64) (result i64)))
  (import "orbit:net/v1" "recv" (func $recv (param i64 i64 i64 i32 i64) (result i64)))
  (import "orbit:net/v1" "send" (func $send (param i64 i64 i64 i32 i64) (result i64)))
  (import "orbit:net/v1" "close" (func $close (param i64) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "127.0.0.1")
  (global $l (mut i64) (i64.const 0))
  (func (export "open") (result i32)
    (global.set $l (call $listen (i64.const 0) (i64.const 9) (i32.const 0) (i32.const 1024) (i32.const 1)))
    (call $local_port (global.get $l)))
  (func (export "serve") (result i64) (local $s i64) (local $n i64) (local $c i64)
    (loop $next
      (local.set $s (call $accept (global.get $l) (i32.const 1000) (i64.const 0)))
      (if (i64.ge_s (local.get $s) (i64.const 0)) (then
        (local.set $n (call $recv (local.get $s) (i64.const 64) (i64.const 1024) (i32.const 5000) (i64.const 0)))
        (if (i64.gt_s (local.get $n) (i64.const 0)) (then
          (drop (call $send (local.get $s) (i64.const 64) (local.get $n) (i32.const 5000) (i64.const 0)))
          (local.set $c (i64.add (local.get $c) (i64.const 1)))))
        (drop (call $close (local.get $s)))
        (br $next))))
    (local.get $c)))"#;

#[tokio::main]
async fn main() {
    let arg = |i: usize, d: u64| {
        std::env::args()
            .nth(i)
            .and_then(|a| a.parse().ok())
            .unwrap_or(d)
    };
    let isolates = arg(1, 1) as usize;
    let clients = arg(2, 8) as usize;
    let secs = arg(3, 5);

    let rt = Runtime::new(
        RuntimeConfig::default(),
        CapabilityRegistry::new().with(NetCapability::new()),
    )
    .unwrap();
    let art = rt.load(SERVER.as_bytes()).unwrap();
    let grants = Grants::none().grant_with(
        CapabilityId::net_v1(),
        net_grant_config(&["127.0.0.1:0-65535".into()], &[]),
    );
    let mut servers = Vec::new();
    let mut ports = Vec::new();
    for _ in 0..isolates {
        let mut iso = rt
            .spawn_isolate(&art, "bench", &grants, Limits::default())
            .await
            .unwrap();
        iso.init().await.unwrap();
        ports.push(iso.invoke("open", &[]).await.unwrap()[0] as u16);
        servers.push(tokio::spawn(async move {
            let n = iso.invoke("serve", &[]).await.unwrap()[0];
            iso.terminate();
            n
        }));
    }

    let stop = Instant::now() + Duration::from_secs(secs);
    let mut tasks = Vec::new();
    for port in &ports {
        for _ in 0..clients {
            let port = *port;
            tasks.push(tokio::spawn(async move {
                let mut lat = Vec::new();
                let msg = [7u8; 64];
                while Instant::now() < stop {
                    let t = Instant::now();
                    let mut c = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
                    c.write_all(&msg).await.unwrap();
                    let mut back = [0u8; 64];
                    c.read_exact(&mut back).await.unwrap();
                    drop(c);
                    lat.push(t.elapsed());
                }
                lat
            }));
        }
    }
    let mut lat = Vec::new();
    for t in tasks {
        lat.extend(t.await.unwrap());
    }
    let mut served = 0;
    for s in servers {
        served += s.await.unwrap();
    }
    lat.sort();
    let pct = |p: f64| lat[((lat.len() as f64 * p) as usize).min(lat.len() - 1)];
    println!(
        "isolates={isolates} clients/isolate={clients} {secs}s: {} requests ({served} served), {:.0} req/s, p50 {:?}, p99 {:?}, max {:?}",
        lat.len(),
        lat.len() as f64 / secs as f64,
        pct(0.5),
        pct(0.99),
        lat.last().unwrap()
    );
}
