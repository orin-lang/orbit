//! `orbit:process/v1` with real WebAssembly: args, env, stdin.

use std::time::Duration;

use orbit_abi::{CapabilityId, status};
use orbit_caps::{
    MemoryStdin, ProcessCapability, ProcessGrant, StdinError, StdinProvider, StdinReader,
};
use orbit_runtime::{
    CapabilityRegistry, Grants, InstanceInfo, InstantiateError, Isolate, Limits, Runtime,
    RuntimeConfig,
};

const WAT: &str = r#"(module
  (import "orbit:process/v1" "arg_count" (func $argc (result i32)))
  (import "orbit:process/v1" "arg" (func $arg (param i32 i64 i64) (result i64)))
  (import "orbit:process/v1" "env_get" (func $env (param i64 i64 i64 i64) (result i64)))
  (import "orbit:process/v1" "stdin_read" (func $stdin (param i64 i64 i64) (result i64)))
  (memory (export "memory") 2)
  (data (i32.const 0) "HOME\00\00\00\00EMPTY\00\00\00FOO=BAR\00")
  (func (export "arg_count") (result i32) (call $argc))
  (func (export "arg") (param i32 i64 i64) (result i64) (call $arg (local.get 0) (local.get 1) (local.get 2)))
  (func (export "env") (param i64 i64 i64 i64) (result i64)
    (call $env (local.get 0) (local.get 1) (local.get 2) (local.get 3)))
  (func (export "stdin") (param i64 i64 i64) (result i64)
    (call $stdin (local.get 0) (local.get 1) (local.get 2)))
  (func (export "byte") (param i64) (result i32) (i32.load8_u (i32.wrap_i64 (local.get 0)))))"#;

const OUT: i64 = 4096;

async fn spawn(rt: &Runtime, grant: &ProcessGrant) -> Isolate {
    let art = rt.load(WAT.as_bytes()).unwrap();
    let g = Grants::none().grant_with(CapabilityId::process_v1(), grant.to_config());
    let mut iso = rt
        .spawn_isolate(&art, "t", &g, Limits::default())
        .await
        .unwrap();
    iso.init().await.unwrap();
    iso
}

async fn call(iso: &mut Isolate, name: &str, args: &[i64]) -> i64 {
    iso.invoke(name, args).await.unwrap()[0]
}

async fn bytes(iso: &mut Isolate, n: i64) -> Vec<u8> {
    let mut v = Vec::new();
    for i in 0..n {
        v.push(call(iso, "byte", &[OUT + i]).await as u8);
    }
    v
}

fn rt(cap: ProcessCapability) -> Runtime {
    Runtime::new(
        RuntimeConfig::default(),
        CapabilityRegistry::new().with(cap),
    )
    .unwrap()
}

fn c(code: i32) -> i64 {
    i64::from(code)
}

#[tokio::test]
async fn args_come_only_from_the_grant() {
    let rt = rt(ProcessCapability::new());
    let mut iso = spawn(&rt, &ProcessGrant::new().args(["prog", "a b", "--x"])).await;
    assert_eq!(call(&mut iso, "arg_count", &[]).await, 3);
    for (i, want) in ["prog", "a b", "--x"].iter().enumerate() {
        let n = call(&mut iso, "arg", &[i as i64, OUT, 64]).await;
        assert_eq!(bytes(&mut iso, n).await, want.as_bytes());
    }
    assert_eq!(
        call(&mut iso, "arg", &[3, OUT, 64]).await,
        c(status::ERR_INVALID)
    );
    assert_eq!(
        call(&mut iso, "arg", &[-1, OUT, 64]).await,
        c(status::ERR_INVALID)
    );
    // Too small: nothing is written.
    assert_eq!(
        call(&mut iso, "arg", &[0, OUT, 2]).await,
        c(status::ERR_TOO_LARGE)
    );
    let mut fresh = spawn(&rt, &ProcessGrant::new().args(["prog"])).await;
    assert_eq!(
        call(&mut fresh, "arg", &[0, OUT, 2]).await,
        c(status::ERR_TOO_LARGE)
    );
    assert_eq!(call(&mut fresh, "byte", &[OUT]).await, 0);
    // Outside memory.
    assert_eq!(
        call(&mut iso, "arg", &[0, 2 * 65536 - 1, 4]).await,
        c(status::ERR_INVALID)
    );
    // No grant args: zero.
    let mut empty = spawn(&rt, &ProcessGrant::new()).await;
    assert_eq!(call(&mut empty, "arg_count", &[]).await, 0);
}

#[tokio::test]
async fn env_lookup_is_exact_and_unset_is_not_found() {
    let rt = rt(ProcessCapability::new());
    let g = ProcessGrant::new()
        .env("HOME", "/h")
        .env("EMPTY", "")
        .env("FOO", "BAR");
    let mut iso = spawn(&rt, &g).await;
    let n = call(&mut iso, "env", &[0, 4, OUT, 64]).await;
    assert_eq!(bytes(&mut iso, n).await, b"/h");
    // Defined but empty is not "unset".
    assert_eq!(call(&mut iso, "env", &[8, 5, OUT, 64]).await, 0);
    // Unset, prefix and case differences.
    assert_eq!(
        call(&mut iso, "env", &[8, 3, OUT, 64]).await,
        c(status::ERR_NOT_FOUND)
    );
    assert_eq!(
        call(&mut iso, "env", &[0, 3, OUT, 64]).await,
        c(status::ERR_NOT_FOUND)
    );
    // `=` in a name and an empty name are invalid.
    assert_eq!(
        call(&mut iso, "env", &[16, 7, OUT, 64]).await,
        c(status::ERR_INVALID)
    );
    assert_eq!(
        call(&mut iso, "env", &[0, 0, OUT, 64]).await,
        c(status::ERR_INVALID)
    );
    assert_eq!(
        call(&mut iso, "env", &[0, 4, OUT, 1]).await,
        c(status::ERR_TOO_LARGE)
    );
    // The host environment is not visible: PATH exists on the host but not here.
    assert_eq!(
        call(&mut iso, "env", &[0, 0, OUT, 64]).await,
        c(status::ERR_INVALID)
    );
}

struct Never;
impl StdinReader for Never {
    fn read(&mut self, _b: &mut [u8], _t: Option<Duration>) -> Result<usize, StdinError> {
        Err(StdinError::Timeout)
    }
}
impl StdinProvider for Never {
    fn open(&self, _i: &InstanceInfo) -> Box<dyn StdinReader> {
        Box::new(Never)
    }
}

#[tokio::test]
async fn stdin_needs_its_own_grant_and_reports_eof_and_timeouts() {
    let rt1 = rt(ProcessCapability::new().with_stdin(MemoryStdin(b"line one\nline two".to_vec())));
    // process/v1 granted without stdin: denied.
    let mut iso = spawn(&rt1, &ProcessGrant::new()).await;
    assert_eq!(
        call(&mut iso, "stdin", &[OUT, 16, 0]).await,
        c(status::ERR_DENIED)
    );
    // With the stdin grant: chunks, then EOF (0).
    let mut iso = spawn(&rt1, &ProcessGrant::new().stdin(true)).await;
    let n = call(&mut iso, "stdin", &[OUT, 9, -1]).await;
    assert_eq!(bytes(&mut iso, n).await, b"line one\n");
    let n = call(&mut iso, "stdin", &[OUT, 64, -1]).await;
    assert_eq!(bytes(&mut iso, n).await, b"line two");
    assert_eq!(call(&mut iso, "stdin", &[OUT, 64, -1]).await, 0);
    assert_eq!(call(&mut iso, "stdin", &[OUT, 64, -1]).await, 0);
    // A bad buffer does not consume input.
    let mut iso = spawn(&rt1, &ProcessGrant::new().stdin(true)).await;
    assert_eq!(
        call(&mut iso, "stdin", &[2 * 65536, 4, 0]).await,
        c(status::ERR_INVALID)
    );
    assert_eq!(call(&mut iso, "stdin", &[OUT, 4, 0]).await, 4);
    // Another instance has its own stream from the start.
    let mut other = spawn(&rt1, &ProcessGrant::new().stdin(true)).await;
    let n = call(&mut other, "stdin", &[OUT, 4, 0]).await;
    assert_eq!(bytes(&mut other, n).await, b"line");
    // Timeout.
    let rt2 = rt(ProcessCapability::new().with_stdin(Never));
    let mut iso = spawn(&rt2, &ProcessGrant::new().stdin(true)).await;
    assert_eq!(
        call(&mut iso, "stdin", &[OUT, 4, 10]).await,
        c(status::ERR_TIMEOUT)
    );
    // The grant flag without a provider on the host is denied too.
    let rt3 = rt(ProcessCapability::new());
    let mut iso = spawn(&rt3, &ProcessGrant::new().stdin(true)).await;
    assert_eq!(
        call(&mut iso, "stdin", &[OUT, 4, 0]).await,
        c(status::ERR_DENIED)
    );
}

#[tokio::test]
async fn ungranted_process_cannot_instantiate_and_state_is_per_instance() {
    let rt = rt(ProcessCapability::new());
    let art = rt.load(WAT.as_bytes()).unwrap();
    let e = rt
        .spawn_isolate(&art, "t", &Grants::none(), Limits::default())
        .await
        .unwrap_err();
    assert!(matches!(e, InstantiateError::NotGranted(ref id) if *id == CapabilityId::process_v1()));
    let mut a = spawn(&rt, &ProcessGrant::new().env("HOME", "A")).await;
    let mut b = spawn(&rt, &ProcessGrant::new().env("HOME", "B")).await;
    let n = call(&mut a, "env", &[0, 4, OUT, 8]).await;
    assert_eq!(bytes(&mut a, n).await, b"A");
    let n = call(&mut b, "env", &[0, 4, OUT, 8]).await;
    assert_eq!(bytes(&mut b, n).await, b"B");
}
