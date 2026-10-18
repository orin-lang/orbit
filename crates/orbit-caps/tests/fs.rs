//! `orbit:fs/v1` with real WebAssembly: mounts, escapes, handles, quotas, listing.

use std::fs;
use std::path::Path;

use orbit_abi::{CapabilityId, status};
use orbit_caps::{FsCapability, FsGrant, MountMode};
use orbit_runtime::{
    CapabilityRegistry, Grants, InstantiateError, Isolate, Limits, Runtime, RuntimeConfig,
};

const MEM_PAGES: u32 = 80; // 5 MiB: room for a 2 MiB read
const SLOT: usize = 256;
const OUT: i64 = 1 << 20; // result buffer
const TABLE: usize = 4096;

fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("\\{b:02x}")).collect()
}

/// A guest with pass-through exports for every `fs/v1` import. `strings` live at
/// `TABLE + i * SLOT`.
fn wat(strings: &[String]) -> String {
    let data: String = strings
        .iter()
        .enumerate()
        .map(|(i, s)| {
            assert!(s.len() < SLOT);
            format!("(data (i32.const {}) \"{}\")\n", TABLE + i * SLOT, hex(s))
        })
        .collect();
    format!(
        r#"(module
  (import "orbit:fs/v1" "open" (func $open (param i64 i64 i32) (result i64)))
  (import "orbit:fs/v1" "read" (func $read (param i64 i64 i64) (result i64)))
  (import "orbit:fs/v1" "write" (func $write (param i64 i64 i64) (result i64)))
  (import "orbit:fs/v1" "flush" (func $flush (param i64) (result i32)))
  (import "orbit:fs/v1" "close" (func $close (param i64) (result i32)))
  (import "orbit:fs/v1" "stat" (func $stat (param i64 i64) (result i32)))
  (import "orbit:fs/v1" "size" (func $size (param i64 i64) (result i64)))
  (import "orbit:fs/v1" "dir_open" (func $dir_open (param i64 i64) (result i64)))
  (import "orbit:fs/v1" "dir_next" (func $dir_next (param i64 i64 i64) (result i64)))
  (import "orbit:fs/v1" "dir_close" (func $dir_close (param i64) (result i32)))
  (import "orbit:fs/v1" "realpath" (func $realpath (param i64 i64 i64 i64) (result i64)))
  (import "orbit:fs/v1" "cwd" (func $cwd (param i64 i64) (result i64)))
  (memory (export "memory") {MEM_PAGES})
  {data}
  (func (export "open") (param i64 i64 i32) (result i64) (call $open (local.get 0) (local.get 1) (local.get 2)))
  (func (export "read") (param i64 i64 i64) (result i64) (call $read (local.get 0) (local.get 1) (local.get 2)))
  (func (export "write") (param i64 i64 i64) (result i64) (call $write (local.get 0) (local.get 1) (local.get 2)))
  (func (export "flush") (param i64) (result i32) (call $flush (local.get 0)))
  (func (export "close") (param i64) (result i32) (call $close (local.get 0)))
  (func (export "stat") (param i64 i64) (result i32) (call $stat (local.get 0) (local.get 1)))
  (func (export "size") (param i64 i64) (result i64) (call $size (local.get 0) (local.get 1)))
  (func (export "dir_open") (param i64 i64) (result i64) (call $dir_open (local.get 0) (local.get 1)))
  (func (export "dir_next") (param i64 i64 i64) (result i64) (call $dir_next (local.get 0) (local.get 1) (local.get 2)))
  (func (export "dir_close") (param i64) (result i32) (call $dir_close (local.get 0)))
  (func (export "realpath") (param i64 i64 i64 i64) (result i64)
    (call $realpath (local.get 0) (local.get 1) (local.get 2) (local.get 3)))
  (func (export "cwd") (param i64 i64) (result i64) (call $cwd (local.get 0) (local.get 1)))
  (func (export "byte") (param i64) (result i32) (i32.load8_u (i32.wrap_i64 (local.get 0)))))"#
    )
}

struct Fx {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
    strings: Vec<String>,
    rt: Runtime,
}

impl Fx {
    fn data(&self) -> std::path::PathBuf {
        self.root.join("data")
    }
    fn work(&self) -> std::path::PathBuf {
        self.root.join("work")
    }
    /// `(ptr, len)` of one of the fixture strings.
    fn s(&self, text: &str) -> [i64; 2] {
        let i = self
            .strings
            .iter()
            .position(|s| s == text)
            .unwrap_or_else(|| panic!("string {text} not in fixture"));
        [(TABLE + i * SLOT) as i64, text.len() as i64]
    }
    fn grant(&self) -> FsGrant {
        FsGrant::new()
            .mount("/data", self.data(), MountMode::ReadOnly)
            .mount("/work", self.work(), MountMode::ReadWrite)
    }
    fn grants(&self, g: &FsGrant) -> Grants {
        Grants::none().grant_with(CapabilityId::fs_v1(), g.to_config().unwrap())
    }
    async fn spawn_with(&self, g: &FsGrant) -> Isolate {
        let art = self.rt.load(wat(&self.strings).as_bytes()).unwrap();
        let mut iso = self
            .rt
            .spawn_isolate(&art, "t", &self.grants(g), Limits::default())
            .await
            .unwrap();
        iso.init().await.unwrap();
        iso
    }
    async fn spawn(&self) -> Isolate {
        self.spawn_with(&self.grant()).await
    }
}

const STRINGS: &[&str] = &[
    "/data/hello.txt",
    "/data/sub/inner.txt",
    "/data/missing",
    "/data/../etc/passwd",
    "/etc/passwd",
    "/data/link_out/secret.txt",
    "/data/abs_out/secret.txt",
    "/data/link_in/inner.txt",
    "/data/link_file_out",
    "/data/dangling",
    "/work/new.txt",
    "/work/app.txt",
    "/data/new.txt",
    "sub/inner.txt",
    "/data",
    "/data/sub",
    "/",
    "/data/link_in",
    "/data/nonexistent/x",
    "/data/hello.txt/x",
    "/work/big.out",
    "/data/big.bin",
    "/work",
    "/data/./sub/../hello.txt",
    "/work/dir",
    "/data/hello\u{0}.txt",
    "",
    "/data/link_out/../hello.txt",
    "/data/sub/nope/deeper",
    "payload",
    "AB",
    "CD",
];

fn fixture_dirs(root: &Path) -> String {
    let data = root.join("data");
    fs::create_dir_all(data.join("sub")).unwrap();
    fs::create_dir_all(root.join("work/dir")).unwrap();
    fs::create_dir_all(root.join("outside")).unwrap();
    fs::write(data.join("hello.txt"), b"hello world").unwrap();
    fs::write(data.join("sub/inner.txt"), b"inner").unwrap();
    fs::write(root.join("outside/secret.txt"), b"TOP SECRET").unwrap();
    fs::write(data.join("big.bin"), vec![7u8; 2 * 1024 * 1024]).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink("../outside", data.join("link_out")).unwrap();
        symlink(root.join("outside"), data.join("abs_out")).unwrap();
        symlink("sub", data.join("link_in")).unwrap();
        symlink("../outside/secret.txt", data.join("link_file_out")).unwrap();
        symlink("nowhere", data.join("dangling")).unwrap();
    }
    root.join("outside/secret.txt").to_str().unwrap().to_owned()
}

async fn fx() -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let host_secret = fixture_dirs(&root);
    let mut strings: Vec<String> = STRINGS.iter().map(|s| (*s).to_owned()).collect();
    strings.push(host_secret);
    let rt = Runtime::new(
        RuntimeConfig::default(),
        CapabilityRegistry::new().with(FsCapability::new()),
    )
    .unwrap();
    Fx {
        _tmp: tmp,
        root,
        strings,
        rt,
    }
}

fn host_secret(f: &Fx) -> [i64; 2] {
    f.s(f.strings.last().unwrap())
}

async fn call(iso: &mut Isolate, name: &str, args: &[i64]) -> i64 {
    iso.invoke(name, args).await.unwrap()[0]
}

async fn open(iso: &mut Isolate, p: [i64; 2], mode: i64) -> i64 {
    call(iso, "open", &[p[0], p[1], mode]).await
}

async fn bytes(iso: &mut Isolate, ptr: i64, len: i64) -> Vec<u8> {
    let mut v = Vec::new();
    for i in 0..len {
        v.push(call(iso, "byte", &[ptr + i]).await as u8);
    }
    v
}

/// Reads a whole small file through stat/open/read like the Orin L0 does.
async fn read_all(iso: &mut Isolate, p: [i64; 2]) -> Result<Vec<u8>, i64> {
    let h = open(iso, p, 0).await;
    if h < 0 {
        return Err(h);
    }
    let mut out = Vec::new();
    loop {
        let n = call(iso, "read", &[h, OUT, 64]).await;
        assert!(n >= 0, "read failed: {n}");
        if n == 0 {
            break;
        }
        out.extend(bytes(iso, OUT, n).await);
    }
    assert_eq!(call(iso, "close", &[h]).await, 0);
    Ok(out)
}

fn c(code: i32) -> i64 {
    i64::from(code)
}

#[tokio::test]
async fn reads_inside_a_mount_and_reports_stat_size_and_errors() {
    let f = fx().await;
    let mut iso = f.spawn().await;
    let hello = f.s("/data/hello.txt");
    assert_eq!(read_all(&mut iso, hello).await.unwrap(), b"hello world");
    assert_eq!(
        read_all(&mut iso, f.s("/data/sub/inner.txt"))
            .await
            .unwrap(),
        b"inner"
    );
    assert_eq!(call(&mut iso, "stat", &hello).await, 1);
    assert_eq!(call(&mut iso, "size", &hello).await, 11);
    assert_eq!(call(&mut iso, "stat", &f.s("/data/sub")).await, 2);
    assert_eq!(call(&mut iso, "stat", &f.s("/data/missing")).await, 0);
    assert_eq!(
        call(&mut iso, "size", &f.s("/data/missing")).await,
        c(status::ERR_NOT_FOUND)
    );
    assert_eq!(
        call(&mut iso, "size", &f.s("/data/sub")).await,
        c(status::ERR_IS_DIR)
    );
    assert_eq!(
        open(&mut iso, f.s("/data/missing"), 0).await,
        c(status::ERR_NOT_FOUND)
    );
    assert_eq!(
        open(&mut iso, f.s("/data/sub"), 0).await,
        c(status::ERR_IS_DIR)
    );
    assert_eq!(
        open(&mut iso, f.s("/data/hello.txt/x"), 0).await,
        c(status::ERR_NOT_DIR)
    );
    assert_eq!(
        open(&mut iso, f.s("/data/hello\u{0}.txt"), 0).await,
        c(status::ERR_INVALID)
    );
    assert_eq!(open(&mut iso, f.s(""), 0).await, c(status::ERR_NOT_FOUND));
    assert_eq!(open(&mut iso, hello, 9).await, c(status::ERR_INVALID));
    // Lexical normalisation inside the mount is fine.
    assert_eq!(
        read_all(&mut iso, f.s("/data/./sub/../hello.txt"))
            .await
            .unwrap(),
        b"hello world"
    );
    // Symlink that stays inside the mount.
    #[cfg(unix)]
    assert_eq!(
        read_all(&mut iso, f.s("/data/link_in/inner.txt"))
            .await
            .unwrap(),
        b"inner"
    );
}

#[tokio::test]
async fn relative_paths_use_the_virtual_cwd() {
    let f = fx().await;
    let mut iso = f.spawn_with(&f.grant().cwd("/data")).await;
    assert_eq!(
        read_all(&mut iso, f.s("sub/inner.txt")).await.unwrap(),
        b"inner"
    );
    let n = call(&mut iso, "cwd", &[OUT, 64]).await;
    assert_eq!(bytes(&mut iso, OUT, n).await, b"/data");
    // Default cwd is "/": a relative path is outside every mount.
    let mut iso = f.spawn().await;
    assert_eq!(
        open(&mut iso, f.s("sub/inner.txt"), 0).await,
        c(status::ERR_DENIED)
    );
    let n = call(&mut iso, "cwd", &[OUT, 64]).await;
    assert_eq!(bytes(&mut iso, OUT, n).await, b"/");
    assert_eq!(
        call(&mut iso, "cwd", &[OUT, 0]).await,
        c(status::ERR_TOO_LARGE)
    );
}

#[tokio::test]
async fn writes_in_a_rw_mount_land_on_the_host() {
    let f = fx().await;
    let mut iso = f.spawn().await;
    let new = f.s("/work/new.txt");
    let h = open(&mut iso, new, 1).await;
    assert!(h > 0);
    let pay = f.s("payload");
    assert_eq!(call(&mut iso, "write", &[h, pay[0], pay[1]]).await, 7);
    assert_eq!(call(&mut iso, "flush", &[h]).await, 0);
    assert_eq!(call(&mut iso, "close", &[h]).await, 0);
    assert_eq!(fs::read(f.work().join("new.txt")).unwrap(), b"payload");
    // Truncate on re-open for write.
    let h = open(&mut iso, new, 1).await;
    let ab = f.s("AB");
    call(&mut iso, "write", &[h, ab[0], ab[1]]).await;
    call(&mut iso, "close", &[h]).await;
    assert_eq!(fs::read(f.work().join("new.txt")).unwrap(), b"AB");
    // Append twice.
    let app = f.s("/work/app.txt");
    for s in ["AB", "CD"] {
        let h = open(&mut iso, app, 2).await;
        let p = f.s(s);
        assert_eq!(call(&mut iso, "write", &[h, p[0], p[1]]).await, 2);
        call(&mut iso, "close", &[h]).await;
    }
    assert_eq!(fs::read(f.work().join("app.txt")).unwrap(), b"ABCD");
    assert_eq!(read_all(&mut iso, app).await.unwrap(), b"ABCD");
    // A read handle cannot write and a write handle cannot read.
    let r = open(&mut iso, app, 0).await;
    assert_eq!(
        call(&mut iso, "write", &[r, pay[0], pay[1]]).await,
        c(status::ERR_INVALID)
    );
    let w = open(&mut iso, app, 2).await;
    assert_eq!(
        call(&mut iso, "read", &[w, OUT, 4]).await,
        c(status::ERR_INVALID)
    );
    // Writing over a directory.
    assert_eq!(
        open(&mut iso, f.s("/work/dir"), 1).await,
        c(status::ERR_IS_DIR)
    );
}

#[tokio::test]
async fn read_only_mount_refuses_writes_without_touching_the_host() {
    let f = fx().await;
    let mut iso = f.spawn().await;
    for mode in [1, 2] {
        assert_eq!(
            open(&mut iso, f.s("/data/new.txt"), mode).await,
            c(status::ERR_DENIED)
        );
        // Even an existing file cannot be opened for writing, and is not truncated.
        assert_eq!(
            open(&mut iso, f.s("/data/hello.txt"), mode).await,
            c(status::ERR_DENIED)
        );
    }
    assert!(!f.data().join("new.txt").exists());
    assert_eq!(
        fs::read(f.data().join("hello.txt")).unwrap(),
        b"hello world"
    );
}

#[tokio::test]
async fn escapes_are_denied() {
    let f = fx().await;
    let mut iso = f.spawn().await;
    let denied = c(status::ERR_DENIED);
    for p in ["/data/../etc/passwd", "/etc/passwd"] {
        assert_eq!(open(&mut iso, f.s(p), 0).await, denied, "{p}");
        assert_eq!(call(&mut iso, "stat", &f.s(p)).await, denied, "{p}");
        assert_eq!(call(&mut iso, "size", &f.s(p)).await, denied, "{p}");
        assert_eq!(call(&mut iso, "dir_open", &f.s(p)).await, denied, "{p}");
        assert_eq!(
            call(&mut iso, "realpath", &[f.s(p)[0], f.s(p)[1], OUT, 256]).await,
            denied,
            "{p}"
        );
    }
    // A host absolute path is just a virtual path outside every mount.
    let hs = host_secret(&f);
    assert_eq!(open(&mut iso, hs, 0).await, denied);
    assert_eq!(call(&mut iso, "stat", &hs).await, denied);
    #[cfg(unix)]
    for p in [
        "/data/link_out/secret.txt",
        "/data/abs_out/secret.txt",
        "/data/link_file_out",
    ] {
        assert_eq!(open(&mut iso, f.s(p), 0).await, denied, "{p}");
        assert_eq!(call(&mut iso, "stat", &f.s(p)).await, denied, "{p}");
        assert_eq!(
            call(&mut iso, "realpath", &[f.s(p)[0], f.s(p)[1], OUT, 256]).await,
            denied,
            "{p}"
        );
    }
    // The escape did not create or read anything.
    assert_eq!(
        fs::read(f.root.join("outside/secret.txt")).unwrap(),
        b"TOP SECRET"
    );
}

#[tokio::test]
async fn symlink_cannot_be_used_to_write_out_of_a_rw_mount() {
    let f = fx().await;
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink("../outside", f.work().join("out")).unwrap();
        symlink(f.root.join("outside"), f.work().join("out_abs")).unwrap();
    }
    let mut strings = f.strings.clone();
    strings.push("/work/out/pwn.txt".into());
    strings.push("/work/out_abs/pwn.txt".into());
    let f = Fx { strings, ..f };
    let mut iso = f.spawn().await;
    #[cfg(unix)]
    for p in ["/work/out/pwn.txt", "/work/out_abs/pwn.txt"] {
        assert_eq!(
            open(&mut iso, f.s(p), 1).await,
            c(status::ERR_DENIED),
            "{p}"
        );
    }
    assert!(!f.root.join("outside/pwn.txt").exists());
    let _ = &mut iso;
}

#[tokio::test]
async fn directory_listing_marks_directories_and_ends_with_zero() {
    let f = fx().await;
    let mut iso = f.spawn().await;
    let h = call(&mut iso, "dir_open", &f.s("/data")).await;
    assert!(h > 0, "{h}");
    let mut seen = Vec::new();
    loop {
        let r = call(&mut iso, "dir_next", &[h, OUT, 255]).await;
        assert!(r >= 0, "{r}");
        if r == 0 {
            break;
        }
        let is_dir = r & orbit_abi::limits::FS_DIR_FLAG != 0;
        let len = r & !orbit_abi::limits::FS_DIR_FLAG;
        let name = String::from_utf8(bytes(&mut iso, OUT, len).await).unwrap();
        seen.push((name, is_dir));
    }
    seen.sort();
    let mut expect = vec![
        ("big.bin".to_owned(), false),
        ("hello.txt".to_owned(), false),
        ("sub".to_owned(), true),
    ];
    #[cfg(unix)]
    expect.extend(
        // Symbolic links are never reported as directories.
        [
            "abs_out",
            "dangling",
            "link_file_out",
            "link_in",
            "link_out",
        ]
        .map(|n| (n.to_owned(), false)),
    );
    expect.sort();
    assert_eq!(seen, expect);
    assert_eq!(call(&mut iso, "dir_next", &[h, OUT, 255]).await, 0);
    assert_eq!(call(&mut iso, "dir_close", &[h]).await, 0);
    assert_eq!(
        call(&mut iso, "dir_next", &[h, OUT, 255]).await,
        c(status::ERR_BAD_HANDLE)
    );
    // A name that does not fit is reported without being consumed.
    let h = call(&mut iso, "dir_open", &f.s("/data/sub")).await;
    assert_eq!(
        call(&mut iso, "dir_next", &[h, OUT, 2]).await,
        c(status::ERR_TOO_LARGE)
    );
    let r = call(&mut iso, "dir_next", &[h, OUT, 64]).await;
    assert_eq!(bytes(&mut iso, OUT, r).await, b"inner.txt");
    assert_eq!(call(&mut iso, "dir_next", &[h, OUT, 64]).await, 0);
    // A file is not a directory; a directory is not a file stream.
    assert_eq!(
        call(&mut iso, "dir_open", &f.s("/data/hello.txt")).await,
        c(status::ERR_NOT_DIR)
    );
    let fh = open(&mut iso, f.s("/data/hello.txt"), 0).await;
    assert_eq!(
        call(&mut iso, "dir_next", &[fh, OUT, 64]).await,
        c(status::ERR_INVALID)
    );
    assert_eq!(
        call(&mut iso, "read", &[h, OUT, 64]).await,
        c(status::ERR_INVALID)
    );
}

#[tokio::test]
async fn ancestors_of_mounts_are_virtual_read_only_directories() {
    let f = fx().await;
    let mut iso = f.spawn().await;
    assert_eq!(call(&mut iso, "stat", &f.s("/")).await, 2);
    let h = call(&mut iso, "dir_open", &f.s("/")).await;
    let mut names = Vec::new();
    loop {
        let r = call(&mut iso, "dir_next", &[h, OUT, 64]).await;
        if r == 0 {
            break;
        }
        assert!(r & orbit_abi::limits::FS_DIR_FLAG != 0);
        let len = r & !orbit_abi::limits::FS_DIR_FLAG;
        names.push(String::from_utf8(bytes(&mut iso, OUT, len).await).unwrap());
    }
    assert_eq!(names, ["data", "work"]);
    assert_eq!(open(&mut iso, f.s("/"), 1).await, c(status::ERR_IS_DIR));
}

#[tokio::test]
async fn realpath_returns_virtual_canonical_paths_only() {
    let f = fx().await;
    let mut iso = f.spawn().await;
    async fn rp(f: &Fx, iso: &mut Isolate, p: &str) -> Result<String, i64> {
        let a = f.s(p);
        let n = call(iso, "realpath", &[a[0], a[1], OUT, 256]).await;
        if n < 0 {
            return Err(n);
        }
        Ok(String::from_utf8(bytes(iso, OUT, n).await).unwrap())
    }
    assert_eq!(
        rp(&f, &mut iso, "/data/./sub/../hello.txt").await.unwrap(),
        "/data/hello.txt"
    );
    assert_eq!(rp(&f, &mut iso, "/data").await.unwrap(), "/data");
    assert_eq!(rp(&f, &mut iso, "/").await.unwrap(), "/");
    // The longest existing prefix is canonical, the rest is kept.
    assert_eq!(
        rp(&f, &mut iso, "/data/nonexistent/x").await.unwrap(),
        "/data/nonexistent/x"
    );
    #[cfg(unix)]
    {
        assert_eq!(
            rp(&f, &mut iso, "/data/link_in").await.unwrap(),
            "/data/sub"
        );
        assert_eq!(
            rp(&f, &mut iso, "/data/link_in/inner.txt").await.unwrap(),
            "/data/sub/inner.txt"
        );
        // A dangling link as the first missing component fails.
        assert_eq!(
            rp(&f, &mut iso, "/data/dangling").await,
            Err(c(status::ERR_NOT_FOUND))
        );
    }
    // Too small a buffer writes nothing.
    let a = f.s("/data/hello.txt");
    assert_eq!(
        call(&mut iso, "realpath", &[a[0], a[1], OUT, 3]).await,
        c(status::ERR_TOO_LARGE)
    );
    // No result ever contains the host root.
    let host_root = f.root.to_str().unwrap().to_owned();
    for p in ["/data/hello.txt", "/work", "/data/sub"] {
        assert!(!rp(&f, &mut iso, p).await.unwrap().contains(&host_root));
    }
}

#[tokio::test]
async fn stale_forged_and_foreign_handles_are_rejected() {
    let f = fx().await;
    let mut a = f.spawn().await;
    let mut b = f.spawn().await;
    let hello = f.s("/data/hello.txt");
    let ha = open(&mut a, hello, 0).await;
    let hb = open(&mut b, hello, 0).await;
    assert!(ha > 0 && hb > 0 && ha != hb, "{ha} {hb}");
    // Another instance's handle value is not usable here.
    let bad = c(status::ERR_BAD_HANDLE);
    assert_eq!(call(&mut a, "read", &[hb, OUT, 8]).await, bad);
    assert_eq!(call(&mut a, "close", &[hb]).await, bad);
    // Stale after close, and after the slot is reused.
    assert_eq!(call(&mut a, "close", &[ha]).await, 0);
    assert_eq!(call(&mut a, "read", &[ha, OUT, 8]).await, bad);
    assert_eq!(call(&mut a, "close", &[ha]).await, bad);
    let ha2 = open(&mut a, hello, 0).await;
    assert_ne!(ha, ha2);
    assert_eq!(call(&mut a, "read", &[ha, OUT, 8]).await, bad);
    // Forged values.
    for forged in [
        0,
        -1,
        1,
        12345,
        i64::MAX,
        i64::MIN,
        ha2 ^ (1 << 20),
        ha2 + 1,
    ] {
        assert_eq!(
            call(&mut a, "read", &[forged, OUT, 8]).await,
            bad,
            "{forged}"
        );
        assert_eq!(
            call(&mut a, "flush", &[forged]).await,
            c(status::ERR_BAD_HANDLE)
        );
        assert_eq!(call(&mut a, "dir_next", &[forged, OUT, 8]).await, bad);
    }
    // The valid handle still works and every handle is positive.
    assert_eq!(call(&mut a, "read", &[ha2, OUT, 5]).await, 5);
}

#[tokio::test]
async fn quotas_cap_handles_bytes_per_call_and_total_writes() {
    let f = fx().await;
    // Open handles (files and directories share the budget).
    let mut iso = f.spawn_with(&f.grant().max_handles(2)).await;
    let hello = f.s("/data/hello.txt");
    let h1 = open(&mut iso, hello, 0).await;
    let h2 = call(&mut iso, "dir_open", &f.s("/data")).await;
    assert!(h1 > 0 && h2 > 0);
    assert_eq!(open(&mut iso, hello, 0).await, c(status::ERR_QUOTA));
    assert_eq!(
        call(&mut iso, "dir_open", &f.s("/data")).await,
        c(status::ERR_QUOTA)
    );
    call(&mut iso, "close", &[h1]).await;
    assert!(open(&mut iso, hello, 0).await > 0);

    // Bytes per call: a 2 MiB file is read 1 MiB at a time.
    let mut iso = f.spawn().await;
    let h = open(&mut iso, f.s("/data/big.bin"), 0).await;
    assert_eq!(call(&mut iso, "read", &[h, OUT, 4 << 20]).await, 1 << 20);
    assert_eq!(call(&mut iso, "read", &[h, OUT, 4 << 20]).await, 1 << 20);
    assert_eq!(call(&mut iso, "read", &[h, OUT, 4 << 20]).await, 0);

    // Total bytes written.
    let mut iso = f.spawn_with(&f.grant().max_write_bytes(10)).await;
    let h = open(&mut iso, f.s("/work/big.out"), 1).await;
    let pay = f.s("payload"); // 7 bytes
    assert_eq!(call(&mut iso, "write", &[h, pay[0], 7]).await, 7);
    assert_eq!(
        call(&mut iso, "write", &[h, pay[0], 7]).await,
        c(status::ERR_QUOTA)
    );
    assert_eq!(call(&mut iso, "write", &[h, pay[0], 3]).await, 3);
    assert_eq!(
        call(&mut iso, "write", &[h, pay[0], 1]).await,
        c(status::ERR_QUOTA)
    );
    call(&mut iso, "close", &[h]).await;
    assert_eq!(fs::read(f.work().join("big.out")).unwrap().len(), 10);
}

#[tokio::test]
async fn bad_guest_pointers_are_invalid_and_do_not_consume_data() {
    let f = fx().await;
    let mut iso = f.spawn().await;
    let h = open(&mut iso, f.s("/data/hello.txt"), 0).await;
    let end = i64::from(MEM_PAGES) * 65536;
    assert_eq!(
        call(&mut iso, "read", &[h, end - 2, 5]).await,
        c(status::ERR_INVALID)
    );
    assert_eq!(
        call(&mut iso, "read", &[h, -1, 5]).await,
        c(status::ERR_INVALID)
    );
    assert_eq!(
        call(&mut iso, "read", &[h, OUT, -1]).await,
        c(status::ERR_INVALID)
    );
    // Nothing was consumed by the failed reads.
    assert_eq!(call(&mut iso, "read", &[h, OUT, 5]).await, 5);
    assert_eq!(bytes(&mut iso, OUT, 5).await, b"hello");
    assert_eq!(
        call(&mut iso, "open", &[end - 1, 8, 0]).await,
        c(status::ERR_INVALID)
    );
    assert_eq!(
        call(&mut iso, "open", &[0, 5000, 0]).await,
        c(status::ERR_TOO_LARGE)
    );
}

#[tokio::test]
async fn no_grant_means_no_link_and_no_mount_means_no_access() {
    let f = fx().await;
    let art = f.rt.load(wat(&f.strings).as_bytes()).unwrap();
    let e =
        f.rt.spawn_isolate(&art, "t", &Grants::none(), Limits::default())
            .await
            .unwrap_err();
    assert!(
        matches!(e, InstantiateError::NotGranted(ref id) if *id == CapabilityId::fs_v1()),
        "{e}"
    );
    // Granted without mounts: linked, but every path is denied.
    let mut iso = f.spawn_with(&FsGrant::new()).await;
    assert_eq!(
        open(&mut iso, f.s("/data/hello.txt"), 0).await,
        c(status::ERR_DENIED)
    );
    // A malformed grant config fails closed.
    let g = Grants::none().grant_with(
        CapabilityId::fs_v1(),
        serde_json::json!({"mounts":[{"guest":"data","host": f.data().to_str().unwrap()}]}),
    );
    let mut iso =
        f.rt.spawn_isolate(&art, "t", &g, Limits::default())
            .await
            .unwrap();
    iso.init().await.unwrap();
    assert_eq!(
        open(&mut iso, f.s("/data/hello.txt"), 0).await,
        c(status::ERR_DENIED)
    );
}

#[test]
fn grant_validation_rejects_bad_mounts() {
    let ro = MountMode::ReadOnly;
    assert!(
        FsGrant::new()
            .mount("data", "/tmp", ro)
            .to_config()
            .is_err()
    );
    assert!(
        FsGrant::new()
            .mount("/a/../b", "/tmp", ro)
            .to_config()
            .is_err()
    );
    assert!(
        FsGrant::new()
            .mount("/a", "/tmp", ro)
            .mount("/a/b", "/tmp", ro)
            .to_config()
            .is_err()
    );
    assert!(
        FsGrant::new()
            .mount("/a", "/tmp", ro)
            .mount("/a", "/tmp", ro)
            .to_config()
            .is_err()
    );
    assert!(
        FsGrant::new()
            .mount("/", "/tmp", ro)
            .mount("/b", "/tmp", ro)
            .to_config()
            .is_err()
    );
    assert!(
        FsGrant::new()
            .mount("/a", "/tmp", ro)
            .mount("/ab", "/tmp", ro)
            .to_config()
            .is_ok()
    );
    assert!(FsGrant::new().cwd("rel").to_config().is_err());
}

#[tokio::test]
async fn instances_do_not_share_state_or_quotas() {
    let f = fx().await;
    let g = f.grant().max_write_bytes(7).max_handles(1);
    let mut a = f.spawn_with(&g).await;
    let mut b = f.spawn_with(&g).await;
    let pay = f.s("payload");
    let ha = open(&mut a, f.s("/work/new.txt"), 1).await;
    assert_eq!(call(&mut a, "write", &[ha, pay[0], 7]).await, 7);
    assert_eq!(
        call(&mut a, "write", &[ha, pay[0], 1]).await,
        c(status::ERR_QUOTA)
    );
    // b still has its full budget and its own handle slot.
    let hb = open(&mut b, f.s("/work/app.txt"), 1).await;
    assert!(hb > 0);
    assert_eq!(call(&mut b, "write", &[hb, pay[0], 7]).await, 7);
    assert_eq!(
        open(&mut a, f.s("/work/app.txt"), 1).await,
        c(status::ERR_QUOTA)
    );
}
