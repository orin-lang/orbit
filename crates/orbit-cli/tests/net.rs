//! `orbit run` with network grants: a WAT server that accepts one connection and echoes it.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// `_start`: listen on 127.0.0.1:{port}, accept one connection, echo one read, exit.
/// Traps if listening fails (so a denied grant is a non-zero exit).
fn server_wat(port: u16) -> String {
    format!(
        r#"(module
  (import "orbit:net/v1" "tcp_listen" (func $listen (param i64 i64 i32 i32 i32) (result i64)))
  (import "orbit:net/v1" "tcp_accept" (func $accept (param i64 i32 i64) (result i64)))
  (import "orbit:net/v1" "recv" (func $recv (param i64 i64 i64 i32 i64) (result i64)))
  (import "orbit:net/v1" "send" (func $send (param i64 i64 i64 i32 i64) (result i64)))
  (import "orbit:net/v1" "close" (func $close (param i64) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "127.0.0.1")
  (func (export "_start") (local $l i64) (local $s i64) (local $n i64)
    (local.set $l (call $listen (i64.const 0) (i64.const 9) (i32.const {port}) (i32.const 16) (i32.const 1)))
    (if (i64.lt_s (local.get $l) (i64.const 0)) (then unreachable))
    (local.set $s (call $accept (local.get $l) (i32.const 20000) (i64.const 0)))
    (if (i64.lt_s (local.get $s) (i64.const 0)) (then unreachable))
    (local.set $n (call $recv (local.get $s) (i64.const 64) (i64.const 64) (i32.const 5000) (i64.const 0)))
    (if (i64.lt_s (local.get $n) (i64.const 0)) (then unreachable))
    (drop (call $send (local.get $s) (i64.const 64) (local.get $n) (i32.const 5000) (i64.const 0)))
    (drop (call $close (local.get $s)))))"#
    )
}

fn write_module(port: u16) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("orbit-net-{}-{port}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("server.wat");
    std::fs::write(&path, server_wat(port)).unwrap();
    path
}

fn orbit(path: &PathBuf, extra: &[&str]) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_orbit"))
        .arg("run")
        .arg(path)
        .args(extra)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("orbit runs")
}

fn connect_retry(port: u16) -> TcpStream {
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => return s,
            Err(e) if Instant::now() < end => {
                let _ = e;
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("cannot connect: {e}"),
        }
    }
}

#[test]
fn run_serves_with_listen_grant() {
    for grant in [
        &["--listen", "127.0.0.1:0-65535"][..],
        &[
            "--grant",
            "net:listen=127.0.0.1:0-65535",
            "--timeout-ms",
            "0",
        ][..],
    ] {
        let port = free_port();
        let path = write_module(port);
        let child = orbit(&path, grant);
        let mut c = connect_retry(port);
        c.write_all(b"ping").unwrap();
        let mut back = [0u8; 4];
        c.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"ping");
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn run_without_net_grant_is_refused() {
    let path = write_module(free_port());
    let out = orbit(&path, &[]).wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("capability not granted: net/v1"));
    // Granted but with a scope that does not cover the address: listen is ERR_DENIED, the
    // guest traps, and no socket was ever bound.
    let port = free_port();
    let path = write_module(port);
    let out = orbit(&path, &["--grant", "net:listen=127.0.0.2:0-65535"])
        .wait_with_output()
        .unwrap();
    assert!(!out.status.success());
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
}

#[test]
fn run_rejects_bad_scopes() {
    let path = write_module(free_port());
    let out = orbit(&path, &["--grant", "net:listen=nonsense"])
        .wait_with_output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("bad --grant"));
}
