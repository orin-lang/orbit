//! Byte access to a shared linear memory: the only module of Orbit that contains `unsafe`.
//!
//! A guest with threads runs on a `wasmtime::SharedMemory`, which other threads write while the
//! host copies. Wasmtime offers no safe accessor for it (`SharedMemory::data` is a slice of
//! `UnsafeCell<u8>`), so the copies below go through relaxed atomic byte operations: a racing
//! guest thread cannot make them undefined behaviour, it can only make the bytes read torn,
//! which is what a guest that races with its own host call must expect.
//!
//! Callers range-check `(offset, len)` against [`SharedMemory::data_size`] first; the functions
//! check again and return `false` rather than panic.
#![allow(unsafe_code)]

use std::sync::atomic::{AtomicU8, Ordering};

use wasmtime::SharedMemory;

/// Copies `out.len()` bytes starting at `offset` out of `mem`. `false` if the range is not
/// inside the memory's current size.
pub(crate) fn read(mem: &SharedMemory, offset: usize, out: &mut [u8]) -> bool {
    let Some(src) = mem.data().get(offset..).and_then(|s| s.get(..out.len())) else {
        return false;
    };
    for (dst, cell) in out.iter_mut().zip(src) {
        // SAFETY: `cell` is a byte of the live mapping (`mem` is borrowed for the whole call
        // and a shared memory never moves or shrinks); `AtomicU8` has the layout of `u8`, and
        // every concurrent access by wasm code is a (possibly non-atomic) wasm load or store,
        // which the threads proposal defines as data races with a defined (non-UB) outcome.
        *dst = unsafe { AtomicU8::from_ptr(cell.get()) }.load(Ordering::Relaxed);
    }
    true
}

/// Copies `data` into `mem` starting at `offset`. `false` if the range is not inside the
/// memory's current size.
pub(crate) fn write(mem: &SharedMemory, offset: usize, data: &[u8]) -> bool {
    let Some(dst) = mem.data().get(offset..).and_then(|s| s.get(..data.len())) else {
        return false;
    };
    for (cell, byte) in dst.iter().zip(data) {
        // SAFETY: as in `read`.
        unsafe { AtomicU8::from_ptr(cell.get()) }.store(*byte, Ordering::Relaxed);
    }
    true
}
