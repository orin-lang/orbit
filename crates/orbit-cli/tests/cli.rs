use std::path::PathBuf;
use std::process::{Command, Output};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn orbit(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_orbit"))
        .args(args)
        .output()
        .expect("orbit runs")
}

fn path(name: &str) -> String {
    fixture(name).to_str().unwrap().to_owned()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn run_denies_capabilities_by_default() {
    let o = orbit(&["run", &path("hello.wat"), "--invoke", "hello"]);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("capability not granted: log/v1"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn run_with_grant_logs_to_stderr() {
    let o = orbit(&[
        "run",
        &path("hello.wat"),
        "--grant",
        "log/v1",
        "--invoke",
        "hello",
        "--tenant",
        "acme",
    ]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("[info] acme/i-1: hello from guest"),
        "{}",
        stderr(&o)
    );
    assert_eq!(stdout(&o).trim(), "0");
}

#[test]
fn run_passes_arguments_and_prints_results() {
    let o = orbit(&[
        "run",
        &path("hello.wat"),
        "--grant",
        "log/v1",
        "--invoke",
        "add",
        "--arg",
        "2",
        "--arg",
        "-5",
    ]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&o).trim(), "-3");
}

#[test]
fn run_reports_fuel_and_timeout_errors() {
    let base = ["run", "--grant", "log/v1", "--invoke", "spin"];
    let m = path("hello.wat");
    let o = orbit(&[
        base[0], &m, base[1], base[2], base[3], base[4], "--fuel", "10000",
    ]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("out of fuel"), "{}", stderr(&o));
    let o = orbit(&[
        base[0],
        &m,
        base[1],
        base[2],
        base[3],
        base[4],
        "--timeout-ms",
        "50",
    ]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("deadline exceeded"), "{}", stderr(&o));
}

#[test]
fn run_rejects_unknown_imports_missing_files_and_bad_flags() {
    let o = orbit(&["run", &path("bad_import.wat")]);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("unknown import `wasi_snapshot_preview1.fd_write`"),
        "{}",
        stderr(&o)
    );

    let o = orbit(&["run", &path("does_not_exist.wasm")]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("cannot read"), "{}", stderr(&o));

    let o = orbit(&["run", &path("hello.wat"), "--grant", "nonsense"]);
    assert!(!o.status.success());
    let o = orbit(&["run", &path("hello.wat"), "--grant", "teleport/v1"]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("no such capability"), "{}", stderr(&o));
}

#[test]
fn run_enforces_memory_limit() {
    let o = orbit(&[
        "run",
        &path("hello.wat"),
        "--grant",
        "log/v1",
        "--max-memory",
        "1000",
    ]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("instantiate failed"), "{}", stderr(&o));
}

#[test]
fn inspect_prints_imports_exports_manifest_and_requirements() {
    let o = orbit(&["inspect", &path("hello.wat")]);
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains("orbit:log/v1.write func (i32, i64, i64) -> i32  [capability log/v1]"),
        "{out}"
    );
    assert!(out.contains("add func (i32, i32) -> i32"), "{out}");
    assert!(
        out.contains("manifest: abi 1 requires [log/v1] producer fixture"),
        "{out}"
    );
    assert!(out.contains("requires: log/v1"), "{out}");
    assert!(out.contains("validation: ok"), "{out}");
}

#[test]
fn inspect_flags_invalid_modules() {
    let o = orbit(&["inspect", &path("bad_import.wat")]);
    assert!(!o.status.success());
    assert!(stdout(&o).contains("validation: FAILED"), "{}", stdout(&o));
}
