#![allow(dead_code)]

use std::sync::Arc;

use orbit_abi::CapabilityId;
use orbit_caps::{LogCapability, MemorySink};
use orbit_runtime::{
    Artifact, CapabilityRegistry, Grants, Isolate, Limits, Runtime, RuntimeConfig,
};

pub fn runtime_with(config: RuntimeConfig) -> (Runtime, MemorySink) {
    let sink = MemorySink::new();
    let registry = CapabilityRegistry::new().with(LogCapability::new(sink.clone()));
    (Runtime::new(config, registry).expect("runtime"), sink)
}

pub fn runtime() -> (Runtime, MemorySink) {
    runtime_with(RuntimeConfig::default())
}

pub fn log_grant() -> Grants {
    Grants::none().grant(CapabilityId::log_v1())
}

pub fn load(rt: &Runtime, wat: &str) -> Arc<Artifact> {
    rt.load(wat.as_bytes()).expect("module loads")
}

/// Spawns with no grants and default limits, and runs init.
pub async fn ready(rt: &Runtime, wat: &str) -> Isolate {
    let art = load(rt, wat);
    let mut iso = rt
        .spawn_isolate(&art, "t", &Grants::none(), Limits::default())
        .await
        .unwrap();
    iso.init().await.unwrap();
    iso
}

/// Increments a global and a memory cell on every `bump`.
pub const COUNTER: &str = r#"
(module
  (memory (export "memory") 1)
  (global $g (mut i32) (i32.const 0))
  (func (export "bump") (result i32)
    (global.set $g (i32.add (global.get $g) (i32.const 1)))
    (i32.store (i32.const 0) (i32.add (i32.load (i32.const 0)) (i32.const 10)))
    (global.get $g))
  (func (export "mem") (result i32) (i32.load (i32.const 0)))
  (func (export "global") (result i32) (global.get $g)))
"#;

pub const SPIN: &str = r#"
(module
  (func (export "spin") (local $i i64)
    (loop $l
      (local.set $i (i64.add (local.get $i) (i64.const 1)))
      (br $l)))
  (func (export "add") (param i32 i32) (result i32) (i32.add (local.get 0) (local.get 1))))
"#;
