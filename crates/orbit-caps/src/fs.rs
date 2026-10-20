//! `orbit:fs/v1`: files and directories under preopened virtual mounts.
//!
//! Authority is a list of **mounts** in the grant: a virtual absolute guest path bound to a host
//! directory, read-only or read-write. Guest paths are virtual: they are normalised lexically
//! (`.` and `..` removed, `..` at the root dropped), a relative path is resolved against the
//! virtual working directory, and the result must lie inside exactly one mount. Inside the mount
//! all work is done with [`cap_std`] relative to a directory handle opened once per instance, so
//! `..` above the mount, absolute symlinks and symlinks leading out of the mount are refused by
//! the OS-level resolution (not by string checks) and report `ERR_DENIED`.
//!
//! Grant config (see [`FsGrant`]): `{"mounts":[{"guest":"/data","host":"/abs/dir","writable":false}],
//! "cwd":"/","max_handles":64,"max_write_bytes":null}`. A malformed config grants no mounts.
//!
//! Host error text and host paths never reach the guest: failures map to status codes only.

use std::any::Any;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use cap_std::ambient_authority;
use cap_std::fs::{Dir, File, OpenOptions, ReadDir};
use orbit_abi::{
    CapabilityId,
    limits::{FS_DEFAULT_MAX_HANDLES, FS_DIR_FLAG, FS_MAX_IO_BYTES, FS_MAX_PATH_BYTES},
    status,
};
use orbit_runtime::{
    Capability, HandleTable, HostState, InstanceInfo, read_guest_bytes, write_guest_bytes,
};
use serde_json::{Value, json};
use wasmtime::{Caller, Linker};

/// How a mount may be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MountMode {
    /// Reads only; any write is `ERR_DENIED` before an OS call.
    ReadOnly,
    /// Reads and writes.
    ReadWrite,
}

/// A builder for the grant configuration of `fs/v1`.
#[derive(Clone, Debug, Default)]
pub struct FsGrant {
    mounts: Vec<(String, PathBuf, MountMode)>,
    cwd: Option<String>,
    max_handles: Option<usize>,
    max_write_bytes: Option<u64>,
}

impl FsGrant {
    /// A grant with no mounts (every path is denied).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a mount of host directory `host` at the virtual path `guest`.
    #[must_use]
    pub fn mount(mut self, guest: &str, host: impl Into<PathBuf>, mode: MountMode) -> Self {
        self.mounts.push((guest.to_owned(), host.into(), mode));
        self
    }

    /// Sets the virtual working directory (default `/`).
    #[must_use]
    pub fn cwd(mut self, cwd: &str) -> Self {
        self.cwd = Some(cwd.to_owned());
        self
    }

    /// Caps the number of open handles (files and directories); default 64.
    #[must_use]
    pub fn max_handles(mut self, n: usize) -> Self {
        self.max_handles = Some(n);
        self
    }

    /// Caps the total bytes the instance may write over its lifetime (default unlimited).
    #[must_use]
    pub fn max_write_bytes(mut self, n: u64) -> Self {
        self.max_write_bytes = Some(n);
        self
    }

    /// Validates the grant and returns its JSON configuration.
    ///
    /// # Errors
    /// A message when a guest path is not absolute or contains `..`, mounts overlap, or the
    /// working directory is not absolute.
    pub fn to_config(&self) -> Result<Value, String> {
        let mut mounts = Vec::new();
        for (g, h, m) in &self.mounts {
            let host = h
                .to_str()
                .ok_or_else(|| "host path is not valid UTF-8".to_owned())?;
            mounts.push(json!({"guest": g, "host": host, "writable": *m == MountMode::ReadWrite}));
        }
        let cfg = json!({
            "mounts": mounts,
            "cwd": self.cwd.clone().unwrap_or_else(|| "/".to_owned()),
            "max_handles": self.max_handles,
            "max_write_bytes": self.max_write_bytes,
        });
        parse_config(&cfg).map(|_| cfg)
    }
}

struct MountCfg {
    guest: Vec<String>,
    guest_text: String,
    host: PathBuf,
    writable: bool,
}

struct Config {
    mounts: Vec<MountCfg>,
    cwd: Vec<String>,
    max_handles: usize,
    max_write_bytes: Option<u64>,
}

/// Splits and normalises a virtual absolute path lexically.
fn normalize(cwd: &[String], path: &str) -> Vec<String> {
    let mut out: Vec<String> = if path.starts_with('/') {
        Vec::new()
    } else {
        cwd.to_vec()
    };
    for c in path.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            c => out.push(c.to_owned()),
        }
    }
    out
}

fn join(components: &[String]) -> String {
    if components.is_empty() {
        "/".to_owned()
    } else {
        components.iter().fold(String::new(), |mut a, c| {
            a.push('/');
            a.push_str(c);
            a
        })
    }
}

fn parse_config(v: &Value) -> Result<Config, String> {
    let mut mounts: Vec<MountCfg> = Vec::new();
    for m in v
        .get("mounts")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
    {
        let guest = m
            .get("guest")
            .and_then(Value::as_str)
            .ok_or("mount without guest path")?;
        let host = m
            .get("host")
            .and_then(Value::as_str)
            .ok_or("mount without host path")?;
        if !guest.starts_with('/') || guest.split('/').any(|c| c == ".." || c == ".") {
            return Err(format!(
                "mount path `{guest}` must be absolute and normalised"
            ));
        }
        if guest.contains('\0') {
            return Err("mount path contains NUL".to_owned());
        }
        let comps = normalize(&[], guest);
        for o in &mounts {
            let n = comps.len().min(o.guest.len());
            if comps[..n] == o.guest[..n] {
                return Err(format!(
                    "mounts `{}` and `{guest}` overlap (nested mounts are not supported)",
                    o.guest_text
                ));
            }
        }
        mounts.push(MountCfg {
            guest_text: join(&comps),
            guest: comps,
            host: PathBuf::from(host),
            writable: m.get("writable").and_then(Value::as_bool).unwrap_or(false),
        });
    }
    let cwd = v.get("cwd").and_then(Value::as_str).unwrap_or("/");
    if !cwd.starts_with('/') {
        return Err("cwd must be absolute".to_owned());
    }
    Ok(Config {
        mounts,
        cwd: normalize(&[], cwd),
        max_handles: v
            .get("max_handles")
            .and_then(Value::as_u64)
            .map_or(FS_DEFAULT_MAX_HANDLES, |n| n as usize),
        max_write_bytes: v.get("max_write_bytes").and_then(Value::as_u64),
    })
}

struct Mount {
    guest: Vec<String>,
    dir: Option<Arc<Dir>>,
    writable: bool,
}

enum OpenEntry {
    File {
        file: File,
        read: bool,
        write: bool,
    },
    Dir {
        iter: DirIter,
        pending: Option<(String, bool)>,
    },
}

enum DirIter {
    Real(ReadDir),
    Virtual(std::vec::IntoIter<String>),
}

/// The state of one isolate's fs capability. Guest threads of one isolate share it (one handle
/// table, one quota); every call holds the lock, so fs calls of an isolate are serialised.
struct FsState(Arc<Mutex<FsInner>>);

struct FsInner {
    mounts: Vec<Mount>,
    cwd: Vec<String>,
    handles: HandleTable<OpenEntry>,
    /// Same bound as the table's; checked first so a refused open touches nothing.
    handle_limit: usize,
    written: u64,
    max_write_bytes: Option<u64>,
}

/// Where a normalised virtual path lands.
enum Place<'a> {
    /// Inside a mount, at this relative host path.
    In(&'a Mount, PathBuf),
    /// A proper ancestor of one or more mounts: a read-only virtual directory.
    Virtual(Vec<String>),
    /// Outside every mount.
    Outside,
}

impl FsInner {
    fn locate(&self, comps: &[String]) -> Place<'_> {
        for m in &self.mounts {
            if comps.len() >= m.guest.len() && comps[..m.guest.len()] == m.guest[..] {
                let rel: PathBuf = comps[m.guest.len()..].iter().collect();
                return Place::In(m, rel);
            }
        }
        let mut children: Vec<String> = Vec::new();
        for m in &self.mounts {
            if m.guest.len() > comps.len() && m.guest[..comps.len()] == comps[..] {
                let name = &m.guest[comps.len()];
                if !children.contains(name) {
                    children.push(name.clone());
                }
            }
        }
        if children.is_empty() {
            Place::Outside
        } else {
            children.sort();
            Place::Virtual(children)
        }
    }
}

fn dot(rel: &Path) -> &Path {
    if rel.as_os_str().is_empty() {
        Path::new(".")
    } else {
        rel
    }
}

/// Maps a host I/O error to a status code. Never forwards text.
fn map_io(e: &io::Error) -> i32 {
    use io::ErrorKind as K;
    match e.kind() {
        K::NotFound => status::ERR_NOT_FOUND,
        K::PermissionDenied => status::ERR_DENIED,
        K::IsADirectory => status::ERR_IS_DIR,
        K::NotADirectory => status::ERR_NOT_DIR,
        K::AlreadyExists => status::ERR_EXISTS,
        K::InvalidInput => status::ERR_INVALID,
        _ => match e.raw_os_error() {
            #[cfg(unix)]
            Some(36) => status::ERR_TOO_LARGE, // ENAMETOOLONG (Linux; same on most unixes)
            _ => status::ERR_IO,
        },
    }
}

fn state(host: &mut HostState, id: &CapabilityId) -> Result<Arc<Mutex<FsInner>>, i32> {
    host.cap_state_mut::<FsState>(id)
        .map(|s| Arc::clone(&s.0))
        .ok_or(status::ERR_IO)
}

fn lock(m: &Mutex<FsInner>) -> MutexGuard<'_, FsInner> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn read_path(caller: &mut Caller<'_, HostState>, ptr: i64, len: i64) -> Result<String, i32> {
    let bytes = read_guest_bytes(caller, ptr, len, FS_MAX_PATH_BYTES)?;
    if bytes.is_empty() {
        return Err(status::ERR_NOT_FOUND);
    }
    if bytes.contains(&0) {
        return Err(status::ERR_INVALID);
    }
    String::from_utf8(bytes).map_err(|_| status::ERR_INVALID)
}

/// Checks that `(ptr, len)` is writable guest memory without writing.
pub(crate) fn check_out(caller: &mut Caller<'_, HostState>, ptr: i64, len: i64) -> Result<(), i32> {
    orbit_runtime::check_guest_range(caller, ptr, len)
}

fn ok_len(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// Copies a result string to the guest, or reports `ERR_TOO_LARGE` writing nothing.
fn put_string(caller: &mut Caller<'_, HostState>, s: &str, ptr: i64, cap: i64, flag: i64) -> i64 {
    if cap < 0 {
        return i64::from(status::ERR_INVALID);
    }
    if s.len() as u64 > cap as u64 {
        return i64::from(status::ERR_TOO_LARGE);
    }
    match write_guest_bytes(caller, ptr, s.as_bytes()) {
        Ok(()) => ok_len(s.len()) | flag,
        Err(c) => i64::from(c),
    }
}

/// Resolves a guest path to `(mount-relative path, writable)` or a status.
/// `want_write` is checked before any OS call.
macro_rules! resolve {
    ($st:expr, $path:expr, $want_write:expr) => {{
        let comps = normalize(&$st.cwd, &$path);
        match $st.locate(&comps) {
            Place::Outside => return status::ERR_DENIED.into(),
            Place::Virtual(c) => (None, c),
            Place::In(m, rel) => {
                if $want_write && !m.writable {
                    return status::ERR_DENIED.into();
                }
                match &m.dir {
                    Some(d) => (Some((Arc::clone(d), rel)), Vec::new()),
                    None => return status::ERR_IO.into(),
                }
            }
        }
    }};
}

fn open(
    id: &CapabilityId,
    caller: &mut Caller<'_, HostState>,
    path_ptr: i64,
    path_len: i64,
    mode: i32,
) -> i64 {
    caller.data_mut().record_call(id);
    if !(0..=2).contains(&mode) {
        return status::ERR_INVALID.into();
    }
    let path = match read_path(caller, path_ptr, path_len) {
        Ok(p) => p,
        Err(c) => return c.into(),
    };
    let sh = match state(caller.data_mut(), id) {
        Ok(s) => s,
        Err(c) => return c.into(),
    };
    let mut st = lock(&sh);
    if st.handles.len() >= st.handle_limit {
        return status::ERR_QUOTA.into();
    }
    let (found, _virt) = resolve!(st, path, mode != 0);
    let Some((dir, rel)) = found else {
        return status::ERR_IS_DIR.into(); // a virtual directory
    };
    let rel = dot(&rel);
    match dir.metadata(rel) {
        Ok(md) if md.is_dir() => return status::ERR_IS_DIR.into(),
        Ok(md) if !md.is_file() => return status::ERR_DENIED.into(), // fifo, device: never opened
        Ok(_) => {}
        Err(e) if mode == 0 || e.kind() != io::ErrorKind::NotFound => return map_io(&e).into(),
        Err(_) => {}
    }
    let mut oo = OpenOptions::new();
    match mode {
        0 => oo.read(true),
        1 => oo.write(true).create(true).truncate(true),
        _ => oo.append(true).create(true),
    };
    let file = match dir.open_with(rel, &oo) {
        Ok(f) => f,
        Err(e) => return map_io(&e).into(),
    };
    match st.handles.insert(OpenEntry::File {
        file,
        read: mode == 0,
        write: mode != 0,
    }) {
        Ok(h) => h,
        Err(_) => status::ERR_QUOTA.into(),
    }
}

fn read(id: &CapabilityId, caller: &mut Caller<'_, HostState>, h: i64, ptr: i64, cap: i64) -> i64 {
    caller.data_mut().record_call(id);
    match state(caller.data_mut(), id) {
        Ok(sh) => match lock(&sh).handles.get(h) {
            Some(OpenEntry::File { read: true, .. }) => {}
            Some(_) => return status::ERR_INVALID.into(),
            None => return status::ERR_BAD_HANDLE.into(),
        },
        Err(c) => return c.into(),
    }
    if cap < 0 {
        return status::ERR_INVALID.into();
    }
    let n = (cap as u64).min(FS_MAX_IO_BYTES) as usize;
    if let Err(c) = check_out(caller, ptr, n as i64) {
        return c.into();
    }
    let mut buf = vec![0u8; n];
    let got = {
        let Ok(sh) = state(caller.data_mut(), id) else {
            return status::ERR_IO.into();
        };
        let mut st = lock(&sh);
        let Some(OpenEntry::File { file, .. }) = st.handles.get_mut(h) else {
            return status::ERR_BAD_HANDLE.into();
        };
        match file.read(&mut buf) {
            Ok(g) => g,
            Err(e) => return map_io(&e).into(),
        }
    };
    match write_guest_bytes(caller, ptr, &buf[..got]) {
        Ok(()) => ok_len(got),
        Err(c) => c.into(),
    }
}

fn write(id: &CapabilityId, caller: &mut Caller<'_, HostState>, h: i64, ptr: i64, len: i64) -> i64 {
    caller.data_mut().record_call(id);
    match state(caller.data_mut(), id) {
        Ok(sh) => match lock(&sh).handles.get(h) {
            Some(OpenEntry::File { write: true, .. }) => {}
            Some(_) => return status::ERR_INVALID.into(),
            None => return status::ERR_BAD_HANDLE.into(),
        },
        Err(c) => return c.into(),
    }
    if len < 0 {
        return status::ERR_INVALID.into();
    }
    let n = (len as u64).min(FS_MAX_IO_BYTES);
    let data = match read_guest_bytes(caller, ptr, n as i64, FS_MAX_IO_BYTES) {
        Ok(d) => d,
        Err(c) => return c.into(),
    };
    let Ok(sh) = state(caller.data_mut(), id) else {
        return status::ERR_IO.into();
    };
    let mut st = lock(&sh);
    if let Some(max) = st.max_write_bytes
        && st.written.saturating_add(n) > max
    {
        return status::ERR_QUOTA.into();
    }
    let Some(OpenEntry::File { file, .. }) = st.handles.get_mut(h) else {
        return status::ERR_BAD_HANDLE.into();
    };
    match file.write(&data) {
        Ok(w) => {
            st.written = st.written.saturating_add(w as u64);
            ok_len(w)
        }
        Err(e) => map_io(&e).into(),
    }
}

fn flush(id: &CapabilityId, caller: &mut Caller<'_, HostState>, h: i64) -> i32 {
    caller.data_mut().record_call(id);
    let sh = match state(caller.data_mut(), id) {
        Ok(s) => s,
        Err(c) => return c,
    };
    let mut st = lock(&sh);
    match st.handles.get_mut(h) {
        Some(OpenEntry::File { file, write, .. }) => {
            if *write && let Err(e) = file.flush() {
                return map_io(&e);
            }
            status::OK
        }
        Some(OpenEntry::Dir { .. }) => status::ERR_INVALID,
        None => status::ERR_BAD_HANDLE,
    }
}

fn close(id: &CapabilityId, caller: &mut Caller<'_, HostState>, h: i64) -> i32 {
    caller.data_mut().record_call(id);
    let sh = match state(caller.data_mut(), id) {
        Ok(s) => s,
        Err(c) => return c,
    };
    let mut st = lock(&sh);
    if st.handles.remove(h).is_some() {
        status::OK
    } else {
        status::ERR_BAD_HANDLE
    }
}

/// `0` none, `1` file (anything that is not a directory), `2` directory; with the size.
fn stat_of(
    id: &CapabilityId,
    caller: &mut Caller<'_, HostState>,
    ptr: i64,
    len: i64,
) -> Result<(i32, u64), i32> {
    caller.data_mut().record_call(id);
    let path = read_path(caller, ptr, len)?;
    let sh = state(caller.data_mut(), id)?;
    let st = lock(&sh);
    let (found, _v) = {
        let comps = normalize(&st.cwd, &path);
        match st.locate(&comps) {
            Place::Outside => return Err(status::ERR_DENIED),
            Place::Virtual(_) => return Ok((2, 0)),
            Place::In(m, rel) => (m.dir.clone().map(|d| (d, rel)), ()),
        }
    };
    let Some((dir, rel)) = found else {
        return Err(status::ERR_IO);
    };
    match dir.metadata(dot(&rel)) {
        Ok(md) if md.is_dir() => Ok((2, 0)),
        Ok(md) => Ok((1, md.len())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok((0, 0)),
        Err(e) => Err(map_io(&e)),
    }
}

fn stat(id: &CapabilityId, caller: &mut Caller<'_, HostState>, ptr: i64, len: i64) -> i32 {
    match stat_of(id, caller, ptr, len) {
        Ok((k, _)) => k,
        Err(c) => c,
    }
}

fn size(id: &CapabilityId, caller: &mut Caller<'_, HostState>, ptr: i64, len: i64) -> i64 {
    match stat_of(id, caller, ptr, len) {
        Ok((1, n)) => i64::try_from(n).unwrap_or(i64::MAX),
        Ok((0, _)) => status::ERR_NOT_FOUND.into(),
        Ok(_) => status::ERR_IS_DIR.into(),
        Err(c) => c.into(),
    }
}

fn dir_open(id: &CapabilityId, caller: &mut Caller<'_, HostState>, ptr: i64, len: i64) -> i64 {
    caller.data_mut().record_call(id);
    let path = match read_path(caller, ptr, len) {
        Ok(p) => p,
        Err(c) => return c.into(),
    };
    let sh = match state(caller.data_mut(), id) {
        Ok(s) => s,
        Err(c) => return c.into(),
    };
    let mut st = lock(&sh);
    if st.handles.len() >= st.handle_limit {
        return status::ERR_QUOTA.into();
    }
    let (found, virt) = resolve!(st, path, false);
    let iter = match found {
        None => DirIter::Virtual(virt.into_iter()),
        Some((dir, rel)) => match dir.read_dir(dot(&rel)) {
            Ok(rd) => DirIter::Real(rd),
            Err(e) => return map_io(&e).into(),
        },
    };
    match st.handles.insert(OpenEntry::Dir {
        iter,
        pending: None,
    }) {
        Ok(h) => h,
        Err(_) => status::ERR_QUOTA.into(),
    }
}

fn dir_next(
    id: &CapabilityId,
    caller: &mut Caller<'_, HostState>,
    h: i64,
    ptr: i64,
    cap: i64,
) -> i64 {
    caller.data_mut().record_call(id);
    let sh = match state(caller.data_mut(), id) {
        Ok(s) => s,
        Err(c) => return c.into(),
    };
    let mut st = lock(&sh);
    let Some(entry) = st.handles.get_mut(h) else {
        return status::ERR_BAD_HANDLE.into();
    };
    let OpenEntry::Dir { iter, pending } = entry else {
        return status::ERR_INVALID.into();
    };
    if cap < 0 {
        return status::ERR_INVALID.into();
    }
    if pending.is_none() {
        loop {
            let next = match iter {
                DirIter::Virtual(it) => match it.next() {
                    Some(n) => (n, true),
                    None => return 0,
                },
                DirIter::Real(rd) => match rd.next() {
                    None => return 0,
                    Some(Err(e)) => return map_io(&e).into(),
                    Some(Ok(de)) => {
                        let Ok(name) = de.file_name().into_string() else {
                            continue; // not representable as a guest string: skipped
                        };
                        let is_dir = match de.file_type() {
                            Ok(t) => t.is_dir(),
                            Err(e) => return map_io(&e).into(),
                        };
                        (name, is_dir)
                    }
                },
            };
            *pending = Some(next);
            break;
        }
    }
    let Some((name, is_dir)) = pending.clone() else {
        return 0;
    };
    // The lock is taken again below to clear `pending`.
    drop(st);
    let flag = if is_dir { FS_DIR_FLAG } else { 0 };
    let r = put_string(caller, &name, ptr, cap, flag);
    if r >= 0
        && let Ok(sh) = state(caller.data_mut(), id)
        && let Some(OpenEntry::Dir { pending, .. }) = lock(&sh).handles.get_mut(h)
    {
        *pending = None;
    }
    r
}

fn realpath(
    id: &CapabilityId,
    caller: &mut Caller<'_, HostState>,
    ptr: i64,
    len: i64,
    out: i64,
    cap: i64,
) -> i64 {
    caller.data_mut().record_call(id);
    let path = match read_path(caller, ptr, len) {
        Ok(p) => p,
        Err(c) => return c.into(),
    };
    let sh = match state(caller.data_mut(), id) {
        Ok(s) => s,
        Err(c) => return c.into(),
    };
    let st = lock(&sh);
    let comps = normalize(&st.cwd, &path);
    let text = match st.locate(&comps) {
        Place::Outside => return status::ERR_DENIED.into(),
        Place::Virtual(_) => join(&comps),
        Place::In(m, rel) => {
            let Some(dir) = m.dir.clone() else {
                return status::ERR_IO.into();
            };
            match canonical(&dir, &rel) {
                Ok(c) => {
                    let mut all = m.guest.clone();
                    all.extend(c);
                    join(&all)
                }
                Err(code) => return code.into(),
            }
        }
    };
    put_string(caller, &text, out, cap, 0)
}

/// The longest existing prefix of `rel` with symlinks resolved (inside the mount), followed
/// by the non-existing rest unchanged.
fn canonical(dir: &Dir, rel: &Path) -> Result<Vec<String>, i32> {
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    for keep in (0..=parts.len()).rev() {
        let prefix: PathBuf = parts[..keep].iter().collect();
        match dir.canonicalize(dot(&prefix)) {
            Ok(c) => {
                if keep < parts.len() {
                    // The first missing component must not be a dangling symlink.
                    let next: PathBuf = parts[..=keep].iter().collect();
                    if dir.symlink_metadata(&next).is_ok() {
                        return Err(status::ERR_NOT_FOUND);
                    }
                }
                let mut out: Vec<String> = c
                    .components()
                    .filter_map(|x| match x {
                        std::path::Component::Normal(n) => Some(n.to_string_lossy().into_owned()),
                        _ => None,
                    })
                    .collect();
                out.extend(parts[keep..].iter().cloned());
                return Ok(out);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound || keep > 0 && is_not_dir(&e) => {}
            Err(e) => return Err(map_io(&e)),
        }
    }
    Err(status::ERR_IO)
}

fn is_not_dir(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotADirectory
}

fn cwd(id: &CapabilityId, caller: &mut Caller<'_, HostState>, out: i64, cap: i64) -> i64 {
    caller.data_mut().record_call(id);
    let text = match state(caller.data_mut(), id) {
        Ok(sh) => join(&lock(&sh).cwd),
        Err(c) => return c.into(),
    };
    put_string(caller, &text, out, cap, 0)
}

/// The `orbit:fs/v1` capability.
#[derive(Clone, Copy, Debug, Default)]
pub struct FsCapability;

impl FsCapability {
    /// A file system capability; mounts come from each isolate's grant.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl fmt::Display for MountMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            MountMode::ReadOnly => "ro",
            MountMode::ReadWrite => "rw",
        })
    }
}

impl Capability for FsCapability {
    fn id(&self) -> CapabilityId {
        CapabilityId::fs_v1()
    }

    fn link(&self, linker: &mut Linker<HostState>) -> wasmtime::Result<()> {
        let id = self.id();
        let m = id.import_module();
        macro_rules! wrap {
            ($name:literal, ($($a:ident: $t:ty),*) -> $r:ty, $f:ident) => {{
                let i = id.clone();
                linker.func_wrap(&m, $name, move |mut c: Caller<'_, HostState>, $($a: $t),*| -> $r {
                    $f(&i, &mut c, $($a),*)
                })?;
            }};
        }
        wrap!("open", (p: i64, l: i64, mode: i32) -> i64, open);
        wrap!("read", (h: i64, p: i64, c: i64) -> i64, read);
        wrap!("write", (h: i64, p: i64, l: i64) -> i64, write);
        wrap!("flush", (h: i64) -> i32, flush);
        wrap!("close", (h: i64) -> i32, close);
        wrap!("stat", (p: i64, l: i64) -> i32, stat);
        wrap!("size", (p: i64, l: i64) -> i64, size);
        wrap!("dir_open", (p: i64, l: i64) -> i64, dir_open);
        wrap!("dir_next", (h: i64, p: i64, c: i64) -> i64, dir_next);
        wrap!("dir_close", (h: i64) -> i32, close);
        wrap!("realpath", (p: i64, l: i64, o: i64, c: i64) -> i64, realpath);
        wrap!("cwd", (o: i64, c: i64) -> i64, cwd);
        Ok(())
    }

    fn instance_state(
        &self,
        info: &InstanceInfo,
        config: &serde_json::Value,
    ) -> Box<dyn Any + Send> {
        // A malformed config grants nothing (fail closed).
        let cfg = parse_config(config).unwrap_or(Config {
            mounts: Vec::new(),
            cwd: Vec::new(),
            max_handles: FS_DEFAULT_MAX_HANDLES,
            max_write_bytes: None,
        });
        let mounts = cfg
            .mounts
            .into_iter()
            .map(|m| Mount {
                dir: Dir::open_ambient_dir(&m.host, ambient_authority())
                    .ok()
                    .map(Arc::new),
                guest: m.guest,
                writable: m.writable,
            })
            .collect::<Vec<_>>();
        Box::new(FsState(Arc::new(Mutex::new(FsInner {
            mounts,
            cwd: cfg.cwd,
            handles: HandleTable::new(info.instance_id.0, cfg.max_handles),
            handle_limit: cfg.max_handles,
            written: 0,
            max_write_bytes: cfg.max_write_bytes,
        }))))
    }

    fn thread_state(
        &self,
        parent: &mut (dyn Any + Send),
        info: &InstanceInfo,
        config: &serde_json::Value,
    ) -> Box<dyn Any + Send> {
        // One handle table, one write quota and one cwd for the whole isolate.
        match parent.downcast_ref::<FsState>() {
            Some(s) => Box::new(FsState(Arc::clone(&s.0))),
            None => self.instance_state(info, config),
        }
    }
}
