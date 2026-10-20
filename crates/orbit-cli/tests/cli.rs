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
fn log_format_plain_splits_streams_and_tagged_goes_to_stderr() {
    let wasm = caps_fixture("wasi_hello.wasm");
    let plain = orbit(&["run", &wasm, "--grant", "wasi_p1/v1"]);
    assert!(plain.status.success(), "{}", stderr(&plain));
    assert!(stdout(&plain).contains("hello from wasi"));
    assert!(stderr(&plain).contains("this goes to stderr"));
    assert!(!stdout(&plain).contains("[info]") && !stderr(&plain).contains("[warn]"));

    let tagged = orbit(&[
        "run",
        &wasm,
        "--grant",
        "wasi_p1/v1",
        "--log-format",
        "tagged",
    ]);
    assert!(tagged.status.success(), "{}", stderr(&tagged));
    assert!(!stdout(&tagged).contains("hello from wasi"));
    assert!(stderr(&tagged).contains("[info] cli/i-1: hello from wasi"));
    assert!(stderr(&tagged).contains("[warn] cli/i-1: this goes to stderr"));
}

#[test]
fn log_format_plain_sends_error_level_to_stderr_unprefixed() {
    let base = ["run", "", "--grant", "log/v1", "--invoke", "levels"];
    let wat = path("levels.wat");
    let mut args = base.to_vec();
    args[1] = &wat;
    let o = orbit(&args);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&o), "out-line\nout-line\n0\n");
    assert_eq!(stderr(&o), "err-line\n");

    args.extend(["--log-format", "tagged"]);
    let o = orbit(&args);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&o), "0\n");
    assert_eq!(
        stderr(&o),
        "[debug] cli/i-1: out-line\n[error] cli/i-1: err-line\n[info] cli/i-1: out-line\n"
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
        "--log-format",
        "tagged",
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
        base[0], &m, base[1], base[2], base[3], base[4], "--fuel", "1000000",
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
        stderr(&o).contains("unknown import `wasi_snapshot_preview1.path_open`"),
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

#[test]
fn run_clock_and_random_grants() {
    let m = path("caps.wat");
    let denied = orbit(&["run", &m, "--invoke", "fill"]);
    assert!(!denied.status.success());
    assert!(
        stderr(&denied).contains("capability not granted"),
        "{}",
        stderr(&denied)
    );

    let grants = ["--grant", "clock/v1", "--grant", "random/v1"];
    let run = |f: &str| {
        let mut a = vec!["run", m.as_str()];
        a.extend(grants);
        a.extend(["--invoke", f]);
        orbit(&a)
    };
    let o = run("fill");
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&o).trim(), "0");
    let o = run("now_ms");
    assert!(o.status.success(), "{}", stderr(&o));
    // After 2020-01-01.
    assert!(stdout(&o).trim().parse::<i64>().unwrap() > 1_577_836_800_000);
    let o = run("mono");
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&o).trim().parse::<i64>().unwrap() % 1_000_000, 0);
}

#[test]
fn run_kv_grant_and_namespaces() {
    let m = path("kv.wat");
    let o = orbit(&["run", &m, "--invoke", "roundtrip"]);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("capability not granted: kv/v1"),
        "{}",
        stderr(&o)
    );

    // Granted but no namespace: open is denied (-2).
    let o = orbit(&["run", &m, "--grant", "kv/v1", "--invoke", "roundtrip"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&o).trim(), "-2");

    let o = orbit(&[
        "run",
        &m,
        "--grant",
        "kv/v1",
        "--kv-namespace",
        "other",
        "--kv-namespace",
        "cache",
        "--invoke",
        "roundtrip",
    ]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&o).trim(), "5");

    // A namespace without the grant is a usage error.
    let o = orbit(&["run", &m, "--kv-namespace", "cache"]);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("--kv-namespace requires --grant kv/v1"),
        "{}",
        stderr(&o)
    );
}

fn caps_fixture(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../orbit-caps/tests/fixtures")
        .join(name)
        .to_str()
        .unwrap()
        .to_owned()
}

#[test]
fn wasi_requires_its_own_grant() {
    let o = orbit(&["run", &caps_fixture("wasi_hello.wasm")]);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("capability not granted: wasi_p1/v1"),
        "{}",
        stderr(&o)
    );
    // The grant is by capability id; the import module name is not a grant spelling that
    // needs special handling, but unknown ids are rejected.
    let o = orbit(&[
        "run",
        &caps_fixture("wasi_hello.wasm"),
        "--grant",
        "wasi/v1",
    ]);
    assert!(stderr(&o).contains("no such capability"), "{}", stderr(&o));
    let o = orbit(&["run", &path("hello.wat"), "--wasi-arg", "x"]);
    assert!(
        stderr(&o).contains("--wasi-arg/--wasi-env require --grant wasi_p1/v1"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn wasi_command_module_runs_start_and_exits_with_the_guest_code() {
    let o = orbit(&["run", &path("command.wat"), "--grant", "wasi_p1/v1"]);
    assert_eq!(o.status.code(), Some(9), "{}", stderr(&o));
    assert_eq!(stdout(&o).trim(), "command", "{}", stderr(&o));
    // --invoke runs that export instead of `_start`.
    let o = orbit(&[
        "run",
        &path("command.wat"),
        "--grant",
        "wasi_p1/v1",
        "--invoke",
        "helper",
    ]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&o).trim(), "5");
}

#[test]
fn rust_program_gets_args_env_and_exit_code() {
    let wasm = caps_fixture("wasi_hello.wasm");
    let o = orbit(&["run", &wasm, "--grant", "wasi_p1/v1"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stdout(&o).contains("hello from wasi"));
    assert!(stdout(&o).contains(r#"args=["main"]"#));
    assert!(stderr(&o).contains("this goes to stderr"));
    assert!(!stderr(&o).contains("[warn]"));

    let o = orbit(&[
        "run",
        &wasm,
        "--grant",
        "wasi_p1/v1",
        "--wasi-arg",
        "exit",
        "--wasi-arg",
        "3",
    ]);
    assert_eq!(o.status.code(), Some(3), "{}", stderr(&o));
    assert!(stdout(&o).contains(r#"args=["main", "exit", "3"]"#));

    let o = orbit(&[
        "run",
        &wasm,
        "--grant",
        "wasi_p1/v1",
        "--wasi-arg",
        "env",
        "--wasi-env",
        "K=v=1",
    ]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(
        stdout(&o).contains(r#"env=[("K", "v=1")]"#),
        "{}",
        stderr(&o)
    );

    let o = orbit(&[
        "run",
        &wasm,
        "--grant",
        "wasi_p1/v1",
        "--wasi-env",
        "novalue",
    ]);
    assert!(stderr(&o).contains("expected K=V"), "{}", stderr(&o));
}

#[test]
fn rust_system_time_fails_without_the_clock_grant() {
    let wasm = caps_fixture("wasi_hello.wasm");
    let base = [
        "run",
        wasm.as_str(),
        "--grant",
        "wasi_p1/v1",
        "--wasi-arg",
        "time",
    ];
    let o = orbit(&base);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(stderr(&o).contains("panicked"), "{}", stderr(&o));
    assert!(stderr(&o).contains("`_start` failed"), "{}", stderr(&o));

    let mut with_clock = base.to_vec();
    with_clock.extend(["--grant", "clock/v1"]);
    let o = orbit(&with_clock);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stdout(&o).contains("unix_secs="), "{}", stderr(&o));
}

#[test]
fn c_program_runs_through_the_cli() {
    let o = orbit(&[
        "run",
        &caps_fixture("wasi_c.wasm"),
        "--grant",
        "wasi_p1/v1",
        "--wasi-arg",
        "e",
        "--wasi-arg",
        "4",
        "--wasi-env",
        "GREETING=hello",
    ]);
    assert_eq!(o.status.code(), Some(4), "{}", stderr(&o));
    assert!(stdout(&o).contains("GREETING=hello"), "{}", stderr(&o));
}

#[test]
fn inspect_shows_the_wasi_capability() {
    let o = orbit(&["inspect", &caps_fixture("wasi_hello.wasm")]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stdout(&o).contains("wasi_snapshot_preview1.fd_write"));
    assert!(stdout(&o).contains("[capability wasi_p1/v1]"));
    assert!(
        stdout(&o).contains("requires: wasi_p1/v1"),
        "{}",
        stdout(&o)
    );
}

fn code(o: &Output) -> i32 {
    o.status.code().expect("exited normally")
}

#[test]
fn run_calls_orin_main_and_exits_with_its_result() {
    let o = orbit(&["run", &path("orin_exit0.wat")]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(
        stdout(&o),
        "",
        "orin_main's result is the exit status, not output"
    );
    let o = orbit(&["run", &path("orin_exit7.wat")]);
    assert_eq!(code(&o), 7, "{}", stderr(&o));
    // 256 would wrap to success; it is clamped to 1 like proc_exit codes.
    let o = orbit(&["run", &path("orin_exit256.wat")]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
}

#[test]
fn orin_trap_uses_exit_status_export_and_prints_the_trap() {
    let o = orbit(&["run", &path("orin_trap5.wat")]);
    assert_eq!(code(&o), 5);
    assert!(stderr(&o).contains("orin_main"), "{}", stderr(&o));
    assert!(stderr(&o).contains("trap"), "{}", stderr(&o));
}

#[test]
fn orin_genuine_trap_exits_1_with_the_original_trap() {
    for name in [
        "orin_trap_m1.wat",
        "orin_trap_noexport.wat",
        "orin_trap_status_traps.wat",
    ] {
        let o = orbit(&["run", &path(name)]);
        assert_eq!(code(&o), 1, "{name}: {}", stderr(&o));
        assert!(
            stderr(&o).contains("`orin_main` failed"),
            "{name}: {}",
            stderr(&o)
        );
        assert!(stderr(&o).contains("trap"), "{name}: {}", stderr(&o));
    }
}

#[test]
fn explicit_invoke_wins_over_orin_main() {
    let o = orbit(&["run", &path("orin_exit7.wat"), "--invoke", "orin_main"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(stdout(&o).trim(), "7");
}

#[test]
fn fuel_flag_enables_metering_and_still_runs_orin_main() {
    let o = orbit(&["run", &path("orin_exit7.wat"), "--fuel", "1000000"]);
    assert_eq!(code(&o), 7, "{}", stderr(&o));
}

fn mount_dir() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("a.txt"), b"hello").unwrap();
    d
}

#[test]
fn fs_and_process_are_denied_without_grants() {
    let o = orbit(&["run", &path("fsproc.wat"), "--invoke", "run"]);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("capability not granted: fs/v1"),
        "{}",
        stderr(&o)
    );
    let d = mount_dir();
    let spec = format!("fs:/d={}", d.path().display());
    let o = orbit(&[
        "run",
        &path("fsproc.wat"),
        "--grant",
        &spec,
        "--invoke",
        "run",
    ]);
    assert!(
        stderr(&o).contains("capability not granted: process/v1"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn fs_mounts_and_process_args_env() {
    let d = mount_dir();
    let spec = format!("fs:/d={}:ro", d.path().display());
    // 5 bytes read, argv = [module, x, y] -> 30, FOO=abc -> 300; process/v1 is granted
    // automatically by --app-arg/--env.
    let o = orbit(&[
        "run",
        &path("fsproc.wat"),
        "--grant",
        &spec,
        "--app-arg",
        "x",
        "--app-arg",
        "y",
        "--env",
        "FOO=abc",
        "--invoke",
        "run",
    ]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&o).trim(), "335");
    // --env-pass copies exactly one host variable.
    let o = Command::new(env!("CARGO_BIN_EXE_orbit"))
        .env("ORBIT_TEST_FOO", "12345678")
        .args([
            "run",
            &path("fsproc.wat"),
            "--grant",
            &spec,
            "--env-pass",
            "ORBIT_TEST_FOO",
            "--invoke",
            "run",
        ])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", stderr(&o));
    // FOO is not defined here: env_get returns ERR_NOT_FOUND (-3): 5 + 10 - 300.
    assert_eq!(stdout(&o).trim(), "-285");
}

#[test]
fn fs_ro_mount_refuses_writes_and_rw_allows_them() {
    let d = mount_dir();
    for (mode, want_ok) in [("ro", false), ("rw", true)] {
        let spec = format!("fs:/d={}:{mode}", d.path().display());
        let o = orbit(&[
            "run",
            &path("fsproc.wat"),
            "--grant",
            &spec,
            "--app-arg",
            "a",
            "--invoke",
            "wr",
        ]);
        assert!(o.status.success(), "{}", stderr(&o));
        let r: i64 = stdout(&o).trim().parse().unwrap();
        assert_eq!(r > 0, want_ok, "{mode}: {r}");
        if !want_ok {
            assert_eq!(r, -2);
        }
    }
    assert!(d.path().join("out.txt").exists());
}

#[test]
fn fs_flags_are_validated() {
    let o = orbit(&[
        "run",
        &path("fsproc.wat"),
        "--grant",
        "fs:/d=/nonexistent-dir-xyz",
        "--invoke",
        "run",
    ]);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("cannot open host directory"),
        "{}",
        stderr(&o)
    );
    assert!(
        !stderr(&o).contains("/nonexistent-dir-xyz:"),
        "{}",
        stderr(&o)
    );
    let o = orbit(&["run", &path("fsproc.wat"), "--cwd", "/", "--invoke", "run"]);
    assert!(stderr(&o).contains("require --grant fs"), "{}", stderr(&o));
    let d = mount_dir();
    let spec = format!("fs:/d={}", d.path().display());
    let o = orbit(&[
        "run",
        &path("fsproc.wat"),
        "--grant",
        &spec,
        "--grant",
        "process:nope",
        "--invoke",
        "run",
    ]);
    assert!(stderr(&o).contains("process:stdin"), "{}", stderr(&o));
}

#[test]
fn stdin_needs_the_process_stdin_grant() {
    use std::io::Write;
    use std::process::Stdio;
    let run = |extra: &[&str]| {
        let mut args = vec!["run", "fsproc_PATH", "--grant", "fs/v1", "--invoke", "in"];
        args.extend(extra);
        let p = path("fsproc.wat");
        let args: Vec<&str> = args
            .iter()
            .map(|a| if *a == "fsproc_PATH" { p.as_str() } else { a })
            .collect();
        let mut child = Command::new(env!("CARGO_BIN_EXE_orbit"))
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"abc").unwrap();
        child.wait_with_output().unwrap()
    };
    let o = run(&["--grant", "process:stdin"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&o).trim(), "3");
    // process/v1 granted for args only: stdin is denied (-2).
    let o = run(&["--app-arg", "x"]);
    assert_eq!(stdout(&o).trim(), "-2");
}

#[test]
fn run_threads_needs_the_grant_and_honours_max_threads() {
    let m = path("threads.wasm");
    let o = orbit(&[
        "run", &m, "--invoke", "count", "--arg", "4", "--arg", "1000", "--grant", "log/v1",
    ]);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("capability not granted: thread/v1"),
        "{}",
        stderr(&o)
    );

    let o = orbit(&[
        "run",
        &m,
        "--invoke",
        "count",
        "--arg",
        "4",
        "--arg",
        "1000",
        "--grant",
        "log/v1",
        "--grant",
        "thread/v1",
    ]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&o).trim(), "4000");

    // max_threads 2: the third spawn fails, so `count` reports -1.
    let o = orbit(&[
        "run",
        &m,
        "--invoke",
        "count",
        "--arg",
        "3",
        "--arg",
        "1000",
        "--grant",
        "log/v1",
        "--max-threads",
        "2",
    ]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&o).trim(), "-1");
}
