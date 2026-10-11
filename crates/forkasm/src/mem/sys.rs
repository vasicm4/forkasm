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

/// Replaces `[addr, addr + len)` with a private anonymous zero mapping.
///
/// # Safety
/// `[addr, addr + len)` must lie inside a reservation owned by the caller,
/// and nothing may hold references into it.
pub(crate) unsafe fn map_anon_fixed(addr: *mut u8, len: usize, rw: bool) -> std::io::Result<()> {
    // SAFETY: forwarded to the caller's contract.
    unsafe {
        mmap_anonymous(
            addr.cast::<c_void>(),
            len,
            prot(rw),
            MapFlags::PRIVATE | MapFlags::FIXED | MapFlags::NORESERVE,
        )?;
    }
    Ok(())
}

/// Maps `fd[off, off + len)` privately (copy-on-write) at `addr`, replacing
/// whatever was there.
///
/// # Safety
/// As for [`map_anon_fixed`].
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
/// As for [`map_anon_fixed`].
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
    if rw {
        ProtFlags::READ | ProtFlags::WRITE
    } else {
        ProtFlags::empty()
    }
}
