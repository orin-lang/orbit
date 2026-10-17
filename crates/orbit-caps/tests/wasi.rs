//! Tests of the restricted WASI preview 1 shim (`wasi_p1/v1`) with hand-written WAT modules.

use std::sync::Arc;

use orbit_abi::CapabilityId;
use orbit_caps::{ClockCapability, ManualClock, MemorySink, WasiCapability};
use orbit_runtime::{
    CapabilityRegistry, ExecError, Grants, InstantiateError, Isolate, IsolateState, Limits,
    LoadError, Runtime, RuntimeConfig,
};
use serde_json::json;

const ESUCCESS: i64 = 0;
const EBADF: i64 = 8;
const EFAULT: i64 = 21;
const EINVAL: i64 = 28;
const ENOSPC: i64 = 51;
const ENOSYS: i64 = 52;
const ENOTSUP: i64 = 58;
const ESPIPE: i64 = 70;
const ENOTCAPABLE: i64 = 76;

/// Every function of the shim, each exported as a raw pass-through, plus memory accessors.
const ALL: &str = r#"
(module
  (import "wasi_snapshot_preview1" "args_get" (func $args_get (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "args_sizes_get" (func $args_sizes_get (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "environ_get" (func $environ_get (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "environ_sizes_get" (func $environ_sizes_get (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "clock_res_get" (func $clock_res_get (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "clock_time_get" (func $clock_time_get (param i32 i64 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_close" (func $fd_close (param i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_fdstat_get" (func $fd_fdstat_get (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_fdstat_set_flags" (func $fd_fdstat_set_flags (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_filestat_get" (func $fd_filestat_get (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_prestat_get" (func $fd_prestat_get (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_prestat_dir_name" (func $fd_prestat_dir_name (param i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_seek" (func $fd_seek (param i32 i64 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "poll_oneoff" (func $poll_oneoff (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
  (import "wasi_snapshot_preview1" "random_get" (func $random_get (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "sched_yield" (func $sched_yield (result i32)))
  (memory (export "memory") 2)
  (func (export "args_get") (param i32 i32) (result i32) (call $args_get (local.get 0) (local.get 1)))
  (func (export "args_sizes_get") (param i32 i32) (result i32) (call $args_sizes_get (local.get 0) (local.get 1)))
  (func (export "environ_get") (param i32 i32) (result i32) (call $environ_get (local.get 0) (local.get 1)))
  (func (export "environ_sizes_get") (param i32 i32) (result i32) (call $environ_sizes_get (local.get 0) (local.get 1)))
  (func (export "clock_res_get") (param i32 i32) (result i32) (call $clock_res_get (local.get 0) (local.get 1)))
  (func (export "clock_time_get") (param i32 i64 i32) (result i32) (call $clock_time_get (local.get 0) (local.get 1) (local.get 2)))
  (func (export "fd_close") (param i32) (result i32) (call $fd_close (local.get 0)))
  (func (export "fd_fdstat_get") (param i32 i32) (result i32) (call $fd_fdstat_get (local.get 0) (local.get 1)))
  (func (export "fd_fdstat_set_flags") (param i32 i32) (result i32) (call $fd_fdstat_set_flags (local.get 0) (local.get 1)))
  (func (export "fd_filestat_get") (param i32 i32) (result i32) (call $fd_filestat_get (local.get 0) (local.get 1)))
  (func (export "fd_prestat_get") (param i32 i32) (result i32) (call $fd_prestat_get (local.get 0) (local.get 1)))
  (func (export "fd_prestat_dir_name") (param i32 i32 i32) (result i32) (call $fd_prestat_dir_name (local.get 0) (local.get 1) (local.get 2)))
  (func (export "fd_read") (param i32 i32 i32 i32) (result i32) (call $fd_read (local.get 0) (local.get 1) (local.get 2) (local.get 3)))
  (func (export "fd_seek") (param i32 i64 i32 i32) (result i32) (call $fd_seek (local.get 0) (local.get 1) (local.get 2) (local.get 3)))
  (func (export "fd_write") (param i32 i32 i32 i32) (result i32) (call $fd_write (local.get 0) (local.get 1) (local.get 2) (local.get 3)))
  (func (export "poll_oneoff") (param i32 i32 i32 i32) (result i32) (call $poll_oneoff (local.get 0) (local.get 1) (local.get 2) (local.get 3)))
  (func (export "proc_exit") (param i32) (call $proc_exit (local.get 0)))
  (func (export "random_get") (param i32 i32) (result i32) (call $random_get (local.get 0) (local.get 1)))
  (func (export "sched_yield") (result i32) (call $sched_yield))
  (func (export "st8") (param i32 i32) (i32.store8 (local.get 0) (local.get 1)))
  (func (export "st32") (param i32 i32) (i32.store (local.get 0) (local.get 1)))
  (func (export "ld8") (param i32) (result i32) (i32.load8_u (local.get 0)))
  (func (export "ld32") (param i32) (result i32) (i32.load (local.get 0)))
  (func (export "ld64") (param i32) (result i64) (i64.load (local.get 0)))
  (func (export "fill") (param i32 i32 i32) (memory.fill (local.get 0) (local.get 1) (local.get 2))))
"#;

const MEM: i64 = 2 * 65536;

struct Guest {
    iso: Isolate,
    sink: MemorySink,
}

struct Setup {
    grants: Grants,
    limits: Limits,
    clock: ManualClock,
    wat: &'static str,
}

impl Setup {
    fn new() -> Self {
        Self {
            grants: Grants::none().grant(CapabilityId::wasi_p1_v1()),
            limits: Limits::default(),
            clock: ManualClock::new(1_700_000_000_123_456_789),
            wat: ALL,
        }
    }

    fn wasi(mut self, config: serde_json::Value) -> Self {
        self.grants = self.grants.grant_with(CapabilityId::wasi_p1_v1(), config);
        self
    }

    fn grant(mut self, id: CapabilityId) -> Self {
        self.grants = self.grants.grant(id);
        self
    }

    fn grant_with(mut self, id: CapabilityId, config: serde_json::Value) -> Self {
        self.grants = self.grants.grant_with(id, config);
        self
    }

    fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    fn wat(mut self, wat: &'static str) -> Self {
        self.wat = wat;
        self
    }

    async fn start(self) -> Guest {
        let mut g = self.start_uninit().await;
        g.iso.init().await.unwrap();
        g
    }

    async fn start_uninit(self) -> Guest {
        let sink = MemorySink::new();
        let rt = runtime(&sink, &self.clock);
        let art = rt.load(self.wat.as_bytes()).unwrap();
        let iso = rt
            .spawn_isolate(&art, "acme", &self.grants, self.limits)
            .await
            .unwrap();
        Guest { iso, sink }
    }
}

fn runtime(sink: &MemorySink, clock: &ManualClock) -> Runtime {
    Runtime::new(
        RuntimeConfig::default(),
        CapabilityRegistry::new()
            .with(WasiCapability::new(sink.clone()).with_time_source(Arc::new(clock.clone()))),
    )
    .unwrap()
}

impl Guest {
    async fn call(&mut self, f: &str, args: &[i64]) -> i64 {
        self.iso
            .invoke(f, args)
            .await
            .unwrap()
            .first()
            .copied()
            .unwrap_or(0)
    }

    async fn st32(&mut self, ptr: i64, v: u32) {
        self.iso.invoke("st32", &[ptr, i64::from(v)]).await.unwrap();
    }

    async fn ld32(&mut self, ptr: i64) -> u32 {
        self.call("ld32", &[ptr]).await as u32
    }

    async fn ld64(&mut self, ptr: i64) -> u64 {
        self.call("ld64", &[ptr]).await as u64
    }

    async fn bytes(&mut self, ptr: i64, len: usize) -> Vec<u8> {
        let mut v = Vec::new();
        for i in 0..len as i64 {
            v.push(self.call("ld8", &[ptr + i]).await as u8);
        }
        v
    }

    async fn put(&mut self, ptr: i64, bytes: &[u8]) {
        for (i, b) in bytes.iter().enumerate() {
            self.iso
                .invoke("st8", &[ptr + i as i64, i64::from(*b)])
                .await
                .unwrap();
        }
    }

    /// Writes `text` with one iovec (iov table at 1024, text at 2048, `nwritten` at 1000).
    async fn write(&mut self, fd: i64, text: &str) -> i64 {
        self.put(2048, text.as_bytes()).await;
        self.st32(1024, 2048).await;
        self.st32(1028, text.len() as u32).await;
        self.call("fd_write", &[fd, 1024, 1, 1000]).await
    }

    fn lines(&self) -> Vec<(String, String)> {
        self.sink
            .records()
            .into_iter()
            .map(|r| (r.level.to_string(), r.message))
            .collect()
    }
}

fn line(level: &str, msg: &str) -> (String, String) {
    (level.to_owned(), msg.to_owned())
}

// --- fd_write ----------------------------------------------------------------------------

#[tokio::test]
async fn stdout_is_info_and_stderr_is_warn_with_host_attributed_records() {
    let mut g = Setup::new().start().await;
    assert_eq!(g.write(1, "out\n").await, ESUCCESS);
    assert_eq!(g.ld32(1000).await, 4);
    assert_eq!(g.write(2, "err\n").await, ESUCCESS);
    assert_eq!(g.lines(), vec![line("info", "out"), line("warn", "err")]);
    let r = &g.sink.records()[0];
    assert_eq!(r.tenant.as_str(), "acme");
    assert_eq!(r.instance, g.iso.id());
}

#[tokio::test]
async fn output_is_line_buffered_and_flushed_when_the_invocation_ends() {
    let mut g = Setup::new().start().await;
    // Each `call` is one invocation; a partial line is flushed when it ends.
    assert_eq!(g.write(1, "par").await, ESUCCESS);
    assert_eq!(g.lines(), vec![line("info", "par")]);

    // Within a single invocation lines are assembled: use an exported function that writes
    // two fragments before returning.
    let wat = r#"
    (module
      (import "wasi_snapshot_preview1" "fd_write" (func $w (param i32 i32 i32 i32) (result i32)))
      (memory (export "memory") 1)
      (data (i32.const 64) "ab")
      (data (i32.const 66) "c\nd\ne")
      (data (i32.const 0) "\40\00\00\00\02\00\00\00")
      (data (i32.const 8) "\42\00\00\00\05\00\00\00")
      (func (export "go") (result i32)
        (drop (call $w (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 100)))
        (drop (call $w (i32.const 1) (i32.const 8) (i32.const 1) (i32.const 100)))
        (i32.const 0)))"#;
    let mut g = Setup::new().wat(wat).start().await;
    assert_eq!(g.call("go", &[]).await, 0);
    assert_eq!(
        g.lines(),
        vec![line("info", "abc"), line("info", "d"), line("info", "e")]
    );
}

#[tokio::test]
async fn multiple_iovecs_are_gathered_and_blank_lines_kept() {
    let mut g = Setup::new().start().await;
    g.put(2048, b"he").await;
    g.put(2060, b"llo\n\nx\n").await;
    for (i, (p, l)) in [(2048u32, 2u32), (2060, 7)].into_iter().enumerate() {
        g.st32(1024 + 8 * i as i64, p).await;
        g.st32(1028 + 8 * i as i64, l).await;
    }
    assert_eq!(g.call("fd_write", &[1, 1024, 2, 1000]).await, ESUCCESS);
    assert_eq!(g.ld32(1000).await, 9);
    assert_eq!(
        g.lines(),
        vec![line("info", "hello"), line("info", ""), line("info", "x")]
    );
}

#[tokio::test]
async fn very_long_lines_are_split_at_the_message_cap() {
    let mut g = Setup::new().start().await;
    // 40_000 zero bytes without a newline.
    g.st32(1024, 4096).await;
    g.st32(1028, 40_000).await;
    assert_eq!(g.call("fd_write", &[1, 1024, 1, 1000]).await, ESUCCESS);
    assert_eq!(g.ld32(1000).await, 40_000);
    let sizes: Vec<_> = g.sink.records().iter().map(|r| r.message.len()).collect();
    assert_eq!(sizes, vec![16384, 16384, 40_000 - 2 * 16384]);
}

#[tokio::test]
async fn bad_fd_and_bad_pointers_are_errnos_not_traps() {
    let mut g = Setup::new().start().await;
    for fd in [0, 3, 4, 100, -1] {
        assert_eq!(g.write(fd, "x\n").await, EBADF, "fd {fd}");
    }
    assert!(g.sink.records().is_empty());

    // iovec array outside memory.
    assert_eq!(g.call("fd_write", &[1, MEM - 4, 1, 1000]).await, EFAULT);
    assert_eq!(g.call("fd_write", &[1, -4, 1, 1000]).await, EFAULT);
    // buffer outside memory.
    g.st32(1024, (MEM - 2) as u32).await;
    g.st32(1028, 8).await;
    assert_eq!(g.call("fd_write", &[1, 1024, 1, 1000]).await, EFAULT);
    // length overflowing the address space.
    g.st32(1024, u32::MAX).await;
    g.st32(1028, u32::MAX).await;
    assert_eq!(g.call("fd_write", &[1, 1024, 1, 1000]).await, EFAULT);
    // too many iovecs.
    assert_eq!(g.call("fd_write", &[1, 1024, 1025, 1000]).await, EINVAL);
    assert_eq!(g.call("fd_write", &[1, 1024, -1, 1000]).await, EINVAL);
    // nwritten outside memory: nothing is emitted.
    g.put(2048, b"lost\n").await;
    g.st32(1024, 2048).await;
    g.st32(1028, 5).await;
    assert_eq!(g.call("fd_write", &[1, 1024, 1, MEM - 2]).await, EFAULT);
    assert!(g.sink.records().is_empty());
    // The isolate is still healthy.
    assert_eq!(g.write(1, "ok\n").await, ESUCCESS);
    assert_eq!(g.iso.state(), IsolateState::Idle);
}

#[tokio::test]
async fn output_counts_against_the_log_budget_per_invocation() {
    // 8 bytes per write; budget 10: full, partial (2), then ENOSPC, all in one invocation.
    let wat = r#"
    (module
      (import "wasi_snapshot_preview1" "fd_write" (func $w (param i32 i32 i32 i32) (result i32)))
      (memory (export "memory") 1)
      (data (i32.const 0) "\10\00\00\00\08\00\00\00")
      (data (i32.const 16) "abcdefg\n")
      (func (export "ld32") (param i32) (result i32) (i32.load (local.get 0)))
      (func (export "thrice") (result i32)
        (drop (call $w (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 100)))
        (drop (call $w (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 104)))
        (call $w (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 108))))"#;
    let mut g = Setup::new()
        .wat(wat)
        .limits(Limits::default().with_max_log_bytes(10))
        .start()
        .await;
    assert_eq!(g.call("thrice", &[]).await, ENOSPC);
    assert_eq!(g.ld32(100).await, 8);
    assert_eq!(g.ld32(104).await, 2);
    assert_eq!(g.ld32(108).await, 0);
    assert_eq!(g.iso.metrics().bytes_logged, 10);
    assert_eq!(g.lines(), vec![line("info", "abcdefg"), line("info", "ab")]);
    // The budget is per invocation: the next one starts fresh.
    assert_eq!(g.call("thrice", &[]).await, ENOSPC);
    assert_eq!(g.iso.metrics().bytes_logged, 20);
}

// --- fd_read, fd_close, fd_seek, fdstat, prestat ------------------------------------------

#[tokio::test]
async fn stdin_is_always_eof() {
    let mut g = Setup::new().start().await;
    g.st32(1000, 99).await;
    g.st32(1024, 4096).await;
    g.st32(1028, 16).await;
    assert_eq!(g.call("fd_read", &[0, 1024, 1, 1000]).await, ESUCCESS);
    assert_eq!(g.ld32(1000).await, 0);
    for fd in [1, 2, 3, -1] {
        assert_eq!(
            g.call("fd_read", &[fd, 1024, 1, 1000]).await,
            EBADF,
            "fd {fd}"
        );
    }
    assert_eq!(g.call("fd_read", &[0, MEM - 4, 1, 1000]).await, EFAULT);
    assert_eq!(g.call("fd_read", &[0, 1024, 1, MEM]).await, EFAULT);
    assert_eq!(g.call("fd_read", &[0, 1024, 5000, 1000]).await, EINVAL);
}

#[tokio::test]
async fn fd_close_flushes_and_makes_the_fd_unusable() {
    let mut g = Setup::new().start().await;
    assert_eq!(g.call("fd_close", &[1]).await, ESUCCESS);
    assert_eq!(g.write(1, "after\n").await, EBADF);
    assert_eq!(g.call("fd_close", &[1]).await, EBADF);
    assert_eq!(g.write(2, "err\n").await, ESUCCESS); // other fds are unaffected
    assert_eq!(g.call("fd_close", &[0]).await, ESUCCESS);
    g.st32(1024, 4096).await;
    assert_eq!(g.call("fd_read", &[0, 1024, 1, 1000]).await, EBADF);
    for fd in [3, 7, -1] {
        assert_eq!(g.call("fd_close", &[fd]).await, EBADF);
    }
    assert_eq!(g.lines(), vec![line("warn", "err")]);
}

#[tokio::test]
async fn seek_fdstat_filestat_and_prestat() {
    let mut g = Setup::new().start().await;
    // stdio is not seekable; other fds do not exist.
    assert_eq!(g.call("fd_seek", &[1, 0, 0, 1000]).await, ESPIPE);
    assert_eq!(g.call("fd_seek", &[3, 0, 0, 1000]).await, EBADF);

    // fdstat: a character device (filetype 2), no flags; stdin reads, stdout/stderr write.
    for (fd, right) in [(0, 1u64 << 1), (1, 1 << 6), (2, 1 << 6)] {
        assert_eq!(g.call("fd_fdstat_get", &[fd, 512]).await, ESUCCESS);
        assert_eq!(g.call("ld8", &[512]).await, 2);
        assert_eq!(g.bytes(514, 2).await, vec![0, 0]);
        let base = g.ld64(520).await;
        assert_ne!(base & right, 0, "fd {fd}");
        assert_eq!(base & (1 << 25 | 1 << 12), 0, "no path or seek rights");
        assert_eq!(g.ld64(528).await, 0);
    }
    assert_eq!(g.call("fd_fdstat_get", &[3, 512]).await, EBADF);
    assert_eq!(g.call("fd_fdstat_get", &[1, MEM - 8]).await, EFAULT);
    assert_eq!(g.call("fd_fdstat_set_flags", &[1, 0]).await, ESUCCESS);
    assert_eq!(g.call("fd_fdstat_set_flags", &[5, 0]).await, EBADF);

    // filestat: character device, size 0.
    g.call("fill", &[512, 0xAA, 64]).await;
    assert_eq!(g.call("fd_filestat_get", &[1, 512]).await, ESUCCESS);
    assert_eq!(g.call("ld8", &[528]).await, 2);
    assert_eq!(g.ld64(544).await, 0);
    assert_eq!(g.call("fd_filestat_get", &[9, 512]).await, EBADF);
    assert_eq!(g.call("fd_filestat_get", &[1, MEM - 8]).await, EFAULT);

    // No preopened directories.
    assert_eq!(g.call("fd_prestat_get", &[3, 512]).await, EBADF);
    assert_eq!(g.call("fd_prestat_get", &[0, 512]).await, EBADF);
    assert_eq!(g.call("fd_prestat_dir_name", &[3, 512, 8]).await, EBADF);
}

// --- args and environment -------------------------------------------------------------------

async fn read_strings(g: &mut Guest, sizes: &str, get: &str) -> (u32, Vec<Vec<u8>>) {
    assert_eq!(g.call(sizes, &[256, 260]).await, ESUCCESS);
    let (count, size) = (g.ld32(256).await, g.ld32(260).await);
    assert_eq!(g.call(get, &[512, 1024]).await, ESUCCESS);
    let mut out = Vec::new();
    for i in 0..count {
        let p = i64::from(g.ld32(512 + 4 * i64::from(i)).await);
        let mut s = Vec::new();
        loop {
            let b = g.call("ld8", &[p + s.len() as i64]).await as u8;
            if b == 0 {
                break;
            }
            s.push(b);
        }
        out.push(s);
    }
    assert_eq!(
        size as usize,
        out.iter().map(|s| s.len() + 1).sum::<usize>(),
        "sizes_get counts the NUL terminators"
    );
    (count, out)
}

#[tokio::test]
async fn argv_defaults_to_main_and_environ_is_empty() {
    let mut g = Setup::new().start().await;
    let (n, args) = read_strings(&mut g, "args_sizes_get", "args_get").await;
    assert_eq!((n, args), (1, vec![b"main".to_vec()]));
    let (n, env) = read_strings(&mut g, "environ_sizes_get", "environ_get").await;
    assert_eq!((n, env), (0, vec![]));
    // The pointer array of an empty environment is not touched.
    assert_eq!(g.call("environ_get", &[512, 1024]).await, ESUCCESS);
}

#[tokio::test]
async fn args_and_env_come_only_from_the_grant_config() {
    let mut g = Setup::new()
        .wasi(json!({
            "args": ["prog", "--flag", "välue"],
            "env": {"B": "2", "A": "1", "bad=key": "x", "NUL": "a\0b", "num": 5}
        }))
        .start()
        .await;
    let (n, args) = read_strings(&mut g, "args_sizes_get", "args_get").await;
    assert_eq!(n, 3);
    assert_eq!(
        args,
        vec![
            b"prog".to_vec(),
            b"--flag".to_vec(),
            "välue".as_bytes().to_vec()
        ]
    );
    // Sorted by key; entries that cannot be represented are dropped.
    let (n, env) = read_strings(&mut g, "environ_sizes_get", "environ_get").await;
    assert_eq!((n, env), (2, vec![b"A=1".to_vec(), b"B=2".to_vec()]));

    // An empty args list means an empty argv (the grant is the complete argv).
    let mut g = Setup::new().wasi(json!({"args": []})).start().await;
    let (n, args) = read_strings(&mut g, "args_sizes_get", "args_get").await;
    assert_eq!((n, args), (0, vec![]));
}

#[tokio::test]
async fn args_pointers_are_bounds_checked() {
    let mut g = Setup::new()
        .wasi(json!({"args": ["a", "b"], "env": {"K": "V"}}))
        .start()
        .await;
    for f in ["args_sizes_get", "environ_sizes_get"] {
        assert_eq!(g.call(f, &[MEM - 2, 0]).await, EFAULT);
        assert_eq!(g.call(f, &[0, MEM]).await, EFAULT);
        assert_eq!(g.call(f, &[-1, 0]).await, EFAULT);
    }
    // argv table and string buffer.
    assert_eq!(g.call("args_get", &[MEM - 4, 0]).await, EFAULT); // table: 2 pointers
    assert_eq!(g.call("args_get", &[0, MEM - 3]).await, EFAULT); // buffer: 4 bytes
    assert_eq!(g.call("environ_get", &[0, MEM - 3]).await, EFAULT);
    assert_eq!(g.call("args_get", &[0, -1]).await, EFAULT);
    assert_eq!(g.iso.state(), IsolateState::Idle);
}

// --- clock and random -----------------------------------------------------------------------

#[tokio::test]
async fn clocks_need_the_clock_grant() {
    let mut g = Setup::new().start().await;
    g.call("fill", &[512, 0xAA, 16]).await;
    for id in [0, 1, 2, 3, 99] {
        assert_eq!(
            g.call("clock_time_get", &[id, 1, 512]).await,
            ENOTCAPABLE,
            "id {id}"
        );
        assert_eq!(
            g.call("clock_res_get", &[id, 512]).await,
            ENOTCAPABLE,
            "id {id}"
        );
    }
    // Nothing was written.
    assert_eq!(g.bytes(512, 16).await, vec![0xAA; 16]);
}

#[tokio::test]
async fn clocks_use_the_clock_semantics_when_granted() {
    let setup = Setup::new().grant(CapabilityId::clock_v1());
    let clock = setup.clock.clone();
    let mut g = setup.start().await;
    // Default resolution is 1 ms (as for orbit:clock/v1).
    assert_eq!(g.call("clock_res_get", &[0, 512]).await, ESUCCESS);
    assert_eq!(g.ld64(512).await, 1_000_000);
    assert_eq!(g.call("clock_time_get", &[0, 1, 512]).await, ESUCCESS);
    assert_eq!(g.ld64(512).await, 1_700_000_000_123_000_000);
    // Monotonic time starts at the instance's creation.
    assert_eq!(g.call("clock_time_get", &[1, 1, 512]).await, ESUCCESS);
    assert_eq!(g.ld64(512).await, 0);
    clock.advance(2_999_999);
    assert_eq!(g.call("clock_time_get", &[1, 1, 512]).await, ESUCCESS);
    assert_eq!(g.ld64(512).await, 2_000_000);
    assert_eq!(g.call("clock_time_get", &[0, 1, 512]).await, ESUCCESS);
    assert_eq!(g.ld64(512).await, 1_700_000_000_126_000_000);

    // Unknown ids, CPU-time clocks, and a bad out pointer.
    assert_eq!(g.call("clock_time_get", &[4, 1, 512]).await, EINVAL);
    assert_eq!(g.call("clock_res_get", &[-1, 512]).await, EINVAL);
    assert_eq!(g.call("clock_time_get", &[2, 1, 512]).await, ENOTSUP);
    assert_eq!(g.call("clock_res_get", &[3, 512]).await, ENOTSUP);
    assert_eq!(g.call("clock_time_get", &[0, 1, MEM - 4]).await, EFAULT);
    assert_eq!(g.call("clock_res_get", &[0, MEM - 4]).await, EFAULT);
}

#[tokio::test]
async fn clock_resolution_comes_from_the_clock_grant() {
    let setup = Setup::new().grant_with(
        CapabilityId::clock_v1(),
        json!({"resolution_ns": 1_000_000_000}),
    );
    let clock = setup.clock.clone();
    let mut g = setup.start().await;
    assert_eq!(g.call("clock_res_get", &[1, 512]).await, ESUCCESS);
    assert_eq!(g.ld64(512).await, 1_000_000_000);
    clock.advance(1_999_999_999);
    assert_eq!(g.call("clock_time_get", &[1, 1, 512]).await, ESUCCESS);
    assert_eq!(g.ld64(512).await, 1_000_000_000);
    assert_eq!(g.call("clock_time_get", &[0, 1, 512]).await, ESUCCESS);
    assert_eq!(g.ld64(512).await % 1_000_000_000, 0);
}

#[tokio::test]
async fn wasi_clock_shares_the_time_source_of_the_clock_capability() {
    let wat = r#"
    (module
      (import "orbit:clock/v1" "now_unix_nanos" (func $now (result i64)))
      (import "wasi_snapshot_preview1" "clock_time_get" (func $t (param i32 i64 i32) (result i32)))
      (memory (export "memory") 1)
      (func (export "orbit") (result i64) (call $now))
      (func (export "wasi") (result i64)
        (drop (call $t (i32.const 0) (i64.const 1) (i32.const 8)))
        (i64.load (i32.const 8))))"#;
    let manual = ManualClock::new(5_000_000_000_000_000);
    let clock = ClockCapability::new(manual.clone());
    let rt = Runtime::new(
        RuntimeConfig::default(),
        CapabilityRegistry::new()
            .with(WasiCapability::new(MemorySink::new()).with_clock(&clock))
            .with(clock),
    )
    .unwrap();
    let art = rt.load(wat.as_bytes()).unwrap();
    let grants = Grants::none()
        .grant(CapabilityId::wasi_p1_v1())
        .grant(CapabilityId::clock_v1());
    let mut iso = rt
        .spawn_isolate(&art, "t", &grants, Limits::default())
        .await
        .unwrap();
    iso.init().await.unwrap();
    manual.advance(123_456_789);
    let a = iso.invoke("orbit", &[]).await.unwrap()[0];
    let b = iso.invoke("wasi", &[]).await.unwrap()[0];
    assert_eq!(a, b);
    assert_eq!(a, 5_000_000_123_000_000);
}

#[tokio::test]
async fn random_needs_the_random_grant() {
    let mut g = Setup::new().start().await;
    g.call("fill", &[512, 0xAA, 16]).await;
    assert_eq!(g.call("random_get", &[512, 16]).await, ENOTCAPABLE);
    assert_eq!(g.call("random_get", &[512, 0]).await, ENOTCAPABLE);
    assert_eq!(g.bytes(512, 16).await, vec![0xAA; 16]);
}

#[tokio::test]
async fn random_fills_checks_bounds_and_limits_size() {
    let mut g = Setup::new().grant(CapabilityId::random_v1()).start().await;
    assert_eq!(g.call("random_get", &[512, 32]).await, ESUCCESS);
    let first = g.bytes(512, 32).await;
    assert_ne!(first, vec![0; 32]);
    assert_eq!(g.bytes(544, 1).await, vec![0], "no write past the length");
    assert_eq!(g.call("random_get", &[512, 32]).await, ESUCCESS);
    assert_ne!(g.bytes(512, 32).await, first);

    assert_eq!(g.call("random_get", &[512, 0]).await, ESUCCESS);
    assert_eq!(g.call("random_get", &[0, 65536]).await, ESUCCESS);
    assert_eq!(g.call("random_get", &[0, 65537]).await, EINVAL);
    assert_eq!(g.call("random_get", &[0, -1]).await, EINVAL);
    assert_eq!(g.call("random_get", &[MEM - 8, 16]).await, EFAULT);
    assert_eq!(g.call("random_get", &[-1, 4]).await, EFAULT);
}

#[tokio::test]
async fn scheduling_functions() {
    let mut g = Setup::new().start().await;
    assert_eq!(g.call("sched_yield", &[]).await, ESUCCESS);
    assert_eq!(g.call("poll_oneoff", &[0, 64, 1, 128]).await, ENOSYS);
    let calls = g.iso.metrics().host_calls[&CapabilityId::wasi_p1_v1()];
    assert_eq!(calls, 2);
}

// --- proc_exit ------------------------------------------------------------------------------

#[tokio::test]
async fn proc_exit_zero_is_a_clean_finish() {
    let mut g = Setup::new().start().await;
    let out = g.iso.invoke("proc_exit", &[0]).await.unwrap();
    assert!(out.is_empty());
    assert_eq!(g.iso.exit_code(), Some(0));
    assert_eq!(g.iso.state(), IsolateState::Terminated);
    assert!(g.iso.failure().is_none());
    assert_eq!(g.iso.metrics().invocations, 1);
    assert_eq!(
        g.iso.invoke("sched_yield", &[]).await,
        Err(ExecError::InvalidState(IsolateState::Terminated))
    );
}

#[tokio::test]
async fn proc_exit_non_zero_fails_the_isolate_with_the_code() {
    for code in [1, 7, 255, -3] {
        let mut g = Setup::new().start().await;
        let err = g.iso.invoke("proc_exit", &[code]).await.unwrap_err();
        assert_eq!(err, ExecError::Exit(code as i32));
        assert_eq!(g.iso.exit_code(), Some(code as i32));
        assert_eq!(g.iso.state(), IsolateState::Failed);
        assert_eq!(g.iso.failure(), Some(&ExecError::Exit(code as i32)));
        g.iso.terminate();
        assert_eq!(g.iso.state(), IsolateState::Terminated);
    }
}

#[tokio::test]
async fn proc_exit_flushes_buffered_output() {
    let wat = r#"
    (module
      (import "wasi_snapshot_preview1" "fd_write" (func $w (param i32 i32 i32 i32) (result i32)))
      (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
      (memory (export "memory") 1)
      (data (i32.const 0) "\10\00\00\00\03\00\00\00")
      (data (i32.const 16) "bye")
      (func (export "go")
        (drop (call $w (i32.const 2) (i32.const 0) (i32.const 1) (i32.const 100)))
        (call $exit (i32.const 5))
        (unreachable)))"#;
    let mut g = Setup::new().wat(wat).start().await;
    assert_eq!(g.iso.invoke("go", &[]).await, Err(ExecError::Exit(5)));
    assert_eq!(g.lines(), vec![line("warn", "bye")]);
}

#[tokio::test]
async fn proc_exit_during_initialisation() {
    let wat = |code: i32| {
        format!(
            r#"(module
              (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
              (func (export "_initialize") (call $exit (i32.const {code}))))"#
        )
    };
    let leak: &'static str = Box::leak(wat(0).into_boxed_str());
    let mut g = Setup::new().wat(leak).start_uninit().await;
    g.iso.init().await.unwrap();
    assert_eq!(g.iso.exit_code(), Some(0));
    assert_eq!(g.iso.state(), IsolateState::Terminated);

    let leak: &'static str = Box::leak(wat(9).into_boxed_str());
    let mut g = Setup::new().wat(leak).start_uninit().await;
    assert_eq!(g.iso.init().await, Err(ExecError::Exit(9)));
    assert_eq!(g.iso.state(), IsolateState::Failed);
}

#[tokio::test]
async fn trap_is_not_confused_with_exit() {
    let wat = r#"
    (module
      (import "wasi_snapshot_preview1" "sched_yield" (func $y (result i32)))
      (func (export "boom") (drop (call $y)) (unreachable)))"#;
    let mut g = Setup::new().wat(wat).start().await;
    assert!(matches!(
        g.iso.invoke("boom", &[]).await,
        Err(ExecError::Trap(_))
    ));
    assert_eq!(g.iso.exit_code(), None);
    assert_eq!(g.iso.state(), IsolateState::Failed);
}

// --- load-time validation and grants --------------------------------------------------------

fn bare_runtime() -> Runtime {
    runtime(&MemorySink::new(), &ManualClock::new(0))
}

#[test]
fn memory64_modules_cannot_import_wasi() {
    let wat = r#"
    (module
      (import "wasi_snapshot_preview1" "fd_write" (func (param i32 i32 i32 i32) (result i32)))
      (memory (export "memory") i64 1))"#;
    let err = bare_runtime().load(wat.as_bytes()).unwrap_err();
    assert!(
        matches!(&err, LoadError::Memory64Unsupported { module, name }
            if module == "wasi_snapshot_preview1" && name == "fd_write"),
        "{err}"
    );
    assert!(err.to_string().contains("wasm32-only"), "{err}");
}

#[test]
fn unimplemented_wasi_functions_fail_at_load() {
    for (name, sig) in [
        (
            "path_open",
            "(param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)",
        ),
        ("sock_accept", "(param i32 i32 i32) (result i32)"),
        ("fd_pwrite", "(param i32 i32 i32 i64 i32) (result i32)"),
    ] {
        let wat = format!(
            r#"(module (import "wasi_snapshot_preview1" "{name}" (func {sig})) (memory (export "memory") 1))"#
        );
        let err = bare_runtime().load(wat.as_bytes()).unwrap_err();
        assert!(
            matches!(&err, LoadError::UnknownImport { module, name: n }
                if module == "wasi_snapshot_preview1" && n == name),
            "{name}: {err}"
        );
    }
    // Other WASI versions and the orbit: spelling of the module are not the shim either.
    for module in [
        "wasi_unstable",
        "wasi_snapshot_preview2",
        "orbit:wasi_p1/v1",
        "wasi:cli/stdout",
    ] {
        let wat = format!(
            r#"(module (import "{module}" "fd_write" (func (param i32 i32 i32 i32) (result i32))) (memory (export "memory") 1))"#
        );
        assert!(matches!(
            bare_runtime().load(wat.as_bytes()),
            Err(LoadError::UnknownImport { .. })
        ));
    }
}

#[test]
fn wasi_signatures_and_imports_are_checked() {
    // i64 where WASI has i32.
    let wat = r#"(module
      (import "wasi_snapshot_preview1" "fd_write" (func (param i64 i32 i32 i32) (result i32)))
      (memory (export "memory") 1))"#;
    assert!(matches!(
        bare_runtime().load(wat.as_bytes()),
        Err(LoadError::SignatureMismatch { .. })
    ));
    // proc_exit returning a value.
    let wat =
        r#"(module (import "wasi_snapshot_preview1" "proc_exit" (func (param i32) (result i32))))"#;
    assert!(matches!(
        bare_runtime().load(wat.as_bytes()),
        Err(LoadError::SignatureMismatch { .. })
    ));
    // A pointer-taking import needs an exported memory; proc_exit alone does not.
    let wat = r#"(module (import "wasi_snapshot_preview1" "fd_write" (func (param i32 i32 i32 i32) (result i32))))"#;
    assert!(matches!(
        bare_runtime().load(wat.as_bytes()),
        Err(LoadError::MissingMemoryExport { .. })
    ));
    let wat = r#"(module (import "wasi_snapshot_preview1" "proc_exit" (func (param i32))))"#;
    bare_runtime().load(wat.as_bytes()).unwrap();
    // Non-function imports from the module.
    let wat = r#"(module (import "wasi_snapshot_preview1" "memory" (memory 1)))"#;
    assert!(matches!(
        bare_runtime().load(wat.as_bytes()),
        Err(LoadError::NonFunctionImport { .. })
    ));
}

#[tokio::test]
async fn wasi_import_without_a_grant_is_not_granted() {
    let rt = bare_runtime();
    let art = rt.load(ALL.as_bytes()).unwrap();
    assert!(art.required().contains(&CapabilityId::wasi_p1_v1()));
    // Granting other capabilities does not help.
    let grants = Grants::none()
        .grant(CapabilityId::clock_v1())
        .grant(CapabilityId::random_v1());
    let err = rt
        .spawn_isolate(&art, "t", &grants, Limits::default())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, InstantiateError::NotGranted(id) if *id == CapabilityId::wasi_p1_v1()),
        "{err}"
    );
    assert!(err.to_string().contains("wasi_p1/v1"));
}

#[test]
fn hosts_without_the_shim_refuse_to_load_wasi_modules() {
    let rt = Runtime::new(RuntimeConfig::default(), CapabilityRegistry::new()).unwrap();
    assert!(matches!(
        rt.load(ALL.as_bytes()),
        Err(LoadError::UnsupportedCapability(id)) if id == CapabilityId::wasi_p1_v1()
    ));
}

#[test]
fn manifest_must_list_wasi_p1() {
    let imports = r#"(import "wasi_snapshot_preview1" "sched_yield" (func (result i32)))"#;
    let bad =
        format!(r#"(module {imports} (@custom "orbit.manifest" "{{\"abi\":1,\"requires\":[]}}"))"#);
    assert!(matches!(
        bare_runtime().load(bad.as_bytes()),
        Err(LoadError::ManifestMismatch { .. })
    ));
    let good = format!(
        r#"(module {imports} (@custom "orbit.manifest" "{{\"abi\":1,\"requires\":[\"wasi_p1/v1\"]}}"))"#
    );
    let art = bare_runtime().load(good.as_bytes()).unwrap();
    assert_eq!(art.required().len(), 1);
}
