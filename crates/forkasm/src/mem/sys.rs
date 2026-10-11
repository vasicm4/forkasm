//! Thin wrappers over the syscalls forkasm needs.

use std::ffi::c_void;
use std::os::fd::{AsFd, OwnedFd};

use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
use rustix::mm::{MapFlags, MprotectFlags, ProtFlags, mmap, mmap_anonymous, mprotect, munmap};

pub(crate) fn memfd(name: &str, size: usize) -> std::io::Result<OwnedFd> {
    let fd = memfd_create(name, MemfdFlags::CLOEXEC)?;
    ftruncate(&fd, size as u64)?;
    Ok(fd)
}

/// Reserves `len` bytes of address space as `PROT_NONE`.
pub(crate) fn reserve(len: usize) -> std::io::Result<*mut u8> {
    // SAFETY: a null hint without MAP_FIXED never replaces existing mappings.
    let p = unsafe {
        mmap_anonymous(
            std::ptr::null_mut(),
            len,
            ProtFlags::empty(),
            MapFlags::PRIVATE | MapFlags::NORESERVE,
        )?
    };
    Ok(p.cast())
}

/// Maps `fd[off, off + len)` privately (copy-on-write) at `addr`, replacing
/// whatever was there.
///
/// # Safety
/// `[addr, addr + len)` must lie inside a reservation owned by the caller,
/// and nothing may hold references into it.
pub(crate) unsafe fn map_file_fixed(
    addr: *mut u8,
    len: usize,
    fd: impl AsFd,
    off: usize,
    rw: bool,
) -> std::io::Result<()> {
    // SAFETY: forwarded to the caller's contract.
    unsafe {
        mmap(
            addr.cast::<c_void>(),
            len,
            prot(rw),
            MapFlags::PRIVATE | MapFlags::FIXED | MapFlags::NORESERVE,
            fd,
            off as u64,
        )?;
    }
    Ok(())
}

/// # Safety
/// As for [`map_file_fixed`].
pub(crate) unsafe fn protect_rw(addr: *mut u8, len: usize) -> std::io::Result<()> {
    if len == 0 {
        return Ok(());
    }
    // SAFETY: forwarded to the caller's contract.
    unsafe { mprotect(addr.cast(), len, MprotectFlags::READ | MprotectFlags::WRITE)? };
    Ok(())
}

/// # Safety
/// `[addr, addr + len)` must be a mapping owned by the caller that nothing
/// references any more.
pub(crate) unsafe fn unmap(addr: *mut u8, len: usize) -> std::io::Result<()> {
    // SAFETY: forwarded to the caller's contract.
    unsafe { munmap(addr.cast(), len)? };
    Ok(())
}

fn prot(rw: bool) -> ProtFlags {
    if rw { ProtFlags::READ | ProtFlags::WRITE } else { ProtFlags::empty() }
}

/// A shared read-write mapping of a whole file, unmapped on drop.
pub(crate) struct SharedMap {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: `SharedMap` owns its mapping; the only access is `write`, whose
// callers write disjoint ranges.
unsafe impl Send for SharedMap {}
// SAFETY: see `Send` above.
unsafe impl Sync for SharedMap {}

impl SharedMap {
    /// Maps `fd[0, len)` shared. With `prefault`, every page is touched now,
    /// so later writes take no faults.
    ///
    /// Prefaulting touches pages one by one rather than using `MAP_POPULATE`:
    /// populate holds the process-wide `mmap_lock` for the whole range, which
    /// stalls every concurrent `mmap` (each fork) for milliseconds, while
    /// ordinary page faults only take a per-VMA lock.
    pub(crate) fn new(fd: impl AsFd, len: usize, prefault: bool) -> std::io::Result<SharedMap> {
        // SAFETY: a null hint without MAP_FIXED never replaces existing mappings.
        let ptr: *mut u8 =
            unsafe { mmap(std::ptr::null_mut(), len, prot(true), MapFlags::SHARED, fd, 0)? }.cast();
        let map = SharedMap { ptr, len };
        if prefault {
            for off in (0..len).step_by(super::PAGE) {
                // SAFETY: inside our fresh mapping, which nothing else uses
                // yet. Writing a zero over zeroed (just-allocated) file pages
                // changes nothing.
                unsafe { std::ptr::write_volatile(ptr.add(off), 0) };
            }
        }
        Ok(map)
    }

    /// Copies `data` into the file at `off`. Concurrent callers must write
    /// disjoint ranges.
    pub(crate) fn write(&self, off: usize, data: &[u8]) {
        assert!(off.checked_add(data.len()).is_some_and(|end| end <= self.len));
        // SAFETY: the range is inside our mapping (checked above); no Rust
        // reference into the mapping exists, and callers never write
        // overlapping ranges concurrently. `data` is outside the mapping
        // (only `write` ever touches it).
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self.ptr.add(off), data.len()) };
    }
}

impl Drop for SharedMap {
    fn drop(&mut self) {
        // SAFETY: we own the mapping and nothing references it any more.
        let _ = unsafe { unmap(self.ptr, self.len) };
    }
}
