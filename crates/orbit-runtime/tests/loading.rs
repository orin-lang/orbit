mod common;

use std::sync::Arc;

use common::*;
use orbit_abi::CapabilityId;
use orbit_runtime::{Grants, InstantiateError, Limits, LoadError};

fn err(rt: &orbit_runtime::Runtime, wat: &str) -> LoadError {
    rt.load(wat.as_bytes()).expect_err("load must fail")
}

#[test]
fn unknown_imports_are_rejected() {
    let (rt, _) = runtime();
    let e = err(
        &rt,
        r#"(module (import "wasi_snapshot_preview1" "path_open"
            (func (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32))))"#,
    );
    assert!(
        matches!(e, LoadError::UnknownImport { ref module, .. } if module == "wasi_snapshot_preview1"),
        "{e}"
    );
    let e = err(&rt, r#"(module (import "env" "foo" (func)))"#);
    assert!(matches!(e, LoadError::UnknownImport { .. }), "{e}");
    // Unknown major version and unknown function of a known capability fail closed.
    let e = err(
        &rt,
        r#"(module (import "orbit:log/v2" "write" (func (param i32 i64 i64) (result i32))))"#,
    );
    assert!(matches!(e, LoadError::UnknownImport { .. }), "{e}");
    let e = err(&rt, r#"(module (import "orbit:log/v1" "flush" (func)))"#);
    assert!(matches!(e, LoadError::UnknownImport { .. }), "{e}");
}

#[test]
fn non_function_imports_are_rejected() {
    let (rt, _) = runtime();
    let e = err(&rt, r#"(module (import "env" "memory" (memory 1)))"#);
    assert!(matches!(e, LoadError::UnknownImport { .. }), "{e}");
    for (decl, kind) in [
        ("(memory 1)", "memory"),
        ("(table 1 funcref)", "table"),
        ("(global i32)", "global"),
    ] {
        let wat = format!(r#"(module (import "orbit:log/v1" "write" {decl}))"#);
        let e = err(&rt, &wat);
        assert!(
            matches!(e, LoadError::NonFunctionImport { kind: k, .. } if k == kind),
            "{e}"
        );
    }
}

#[test]
fn signature_mismatch_is_rejected() {
    let (rt, _) = runtime();
    for sig in [
        "(param i32 i32 i32) (result i32)",
        "(param i32 i64 i64)",
        "(param i32 i64 i64) (result i64)",
        "(param i64 i64 i64) (result i32)",
        "",
    ] {
        let wat = format!(
            r#"(module (import "orbit:log/v1" "write" (func {sig})) (memory (export "memory") 1))"#
        );
        let e = err(&rt, &wat);
        assert!(
            matches!(e, LoadError::SignatureMismatch { .. }),
            "{sig}: {e}"
        );
    }
}

#[test]
fn pointer_imports_need_exported_memory() {
    let (rt, _) = runtime();
    let e = err(
        &rt,
        r#"(module (import "orbit:log/v1" "write" (func (param i32 i64 i64) (result i32))))"#,
    );
    assert!(matches!(e, LoadError::MissingMemoryExport { .. }), "{e}");
}

#[test]
fn capability_known_to_abi_but_not_served_is_unsupported() {
    let (rt, _) = runtime();
    let e = err(
        &rt,
        r#"(module (import "orbit:clock/v1" "now_unix_nanos" (func (result i64))))"#,
    );
    assert!(
        matches!(e, LoadError::UnsupportedCapability(ref id) if *id == CapabilityId::clock_v1()),
        "{e}"
    );
}

const LOG_IMPORT: &str = r#"(import "orbit:log/v1" "write" (func (param i32 i64 i64) (result i32)))
  (memory (export "memory") 1)"#;

#[test]
fn manifest_must_list_imports() {
    let (rt, _) = runtime();
    let wat = format!(
        r#"(module {LOG_IMPORT} (@custom "orbit.manifest" "{{\"abi\":1,\"requires\":[]}}"))"#
    );
    let e = err(&rt, &wat);
    assert!(
        matches!(&e, LoadError::ManifestMismatch { missing } if missing == &[CapabilityId::log_v1()]),
        "{e}"
    );
}

#[test]
fn malformed_manifests_are_rejected() {
    let (rt, _) = runtime();
    for body in [
        r#"not json"#,
        r#"{\"abi\":2,\"requires\":[]}"#,
        r#"{\"abi\":1,\"requires\":[\"bogus\"]}"#,
    ] {
        let wat = format!(r#"(module (@custom "orbit.manifest" "{body}"))"#);
        assert!(matches!(err(&rt, &wat), LoadError::Manifest(_)), "{body}");
    }
    let wat = r#"(module (@custom "orbit.manifest" "{\"abi\":1}") (@custom "orbit.manifest" "{\"abi\":1}"))"#;
    assert!(matches!(err(&rt, wat), LoadError::DuplicateManifest));
    assert!(matches!(
        err(&rt, "(module (bogus))"),
        LoadError::Invalid(_)
    ));
    assert!(matches!(
        rt.load(b"\0asm\x01\0\0\0garbage").unwrap_err(),
        LoadError::Compile(_)
    ));
}

#[tokio::test]
async fn manifest_requirements_still_need_a_grant() {
    let (rt, _) = runtime();
    // The manifest lists log/v1 although the module does not import it, and kv/v1 which this
    // host does not serve at all.
    let wat = r#"(module (@custom "orbit.manifest" "{\"abi\":1,\"requires\":[\"log/v1\",\"kv/v1\"],\"producer\":\"test\"}"))"#;
    let art = load(&rt, wat);
    assert_eq!(art.manifest().unwrap().producer.as_deref(), Some("test"));
    assert!(art.imported().is_empty());
    assert_eq!(art.required().len(), 2);

    let e = rt
        .spawn_isolate(&art, "t", &Grants::none(), Limits::default())
        .await
        .unwrap_err();
    assert!(matches!(e, InstantiateError::NotGranted(_)), "{e}");
    let e = rt
        .spawn_isolate(&art, "t", &log_grant(), Limits::default())
        .await
        .unwrap_err();
    assert!(
        matches!(e, InstantiateError::NotGranted(ref id) if *id == CapabilityId::kv_v1()),
        "{e}"
    );
    let both = log_grant().grant(CapabilityId::kv_v1());
    let e = rt
        .spawn_isolate(&art, "t", &both, Limits::default())
        .await
        .unwrap_err();
    assert!(
        matches!(e, InstantiateError::Unavailable(ref id) if *id == CapabilityId::kv_v1()),
        "{e}"
    );
    assert_eq!(
        rt.reserved_memory_bytes(),
        0,
        "failed spawns reserve nothing"
    );
}

#[test]
fn artifacts_are_cached_by_content_hash() {
    let (rt, _) = runtime();
    let a = rt.load(COUNTER.as_bytes()).unwrap();
    let b = rt.load(COUNTER.as_bytes()).unwrap();
    assert!(Arc::ptr_eq(&a, &b));
    // The binary form of the same module hits the same cache entry.
    let wasm = wat::parse_str(COUNTER).unwrap();
    let c = rt.load(&wasm).unwrap();
    assert!(Arc::ptr_eq(&a, &c));
    assert_eq!(rt.cached_artifacts(), 1);
    rt.load(SPIN.as_bytes()).unwrap();
    assert_eq!(rt.cached_artifacts(), 2);
    assert!(rt.load(b"(module (bogus))").is_err());
    assert_eq!(rt.cached_artifacts(), 2);
}

#[test]
fn inspect_reports_imports_and_exports() {
    let (rt, _) = runtime();
    let art = load(
        &rt,
        &format!("(module {LOG_IMPORT} (func (export \"f\") (param i32)))"),
    );
    let info = art.info();
    assert_eq!(info.imports.len(), 1);
    assert_eq!(
        info.imports[0].signature.as_deref(),
        Some("(i32, i64, i64) -> i32")
    );
    assert!(
        info.exports
            .iter()
            .any(|e| e.name == "f" && e.signature.as_deref() == Some("(i32)"))
    );
    assert_eq!(
        art.required()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["log/v1"]
    );
}
