//! Write tracking with userfaultfd in asynchronous write-protect mode.
//!
//! A memory registered here is write-protected page by page: a write to a
//! protected page is resolved by the kernel itself (no fault is delivered to
//! us) and simply leaves the page unprotected. `PAGEMAP_SCAN` with
//! `PM_SCAN_WP_MATCHING` then reports the unprotected ("written") pages and
//! re-protects them atomically, so each scan returns the pages written since
//! the previous one. Requires Linux ≥ 6.7.
//!
//! One userfaultfd serves the whole process: registrations belong to the
//! mappings, and live until the mapping is unmapped or replaced.
//!
//! The same fd also serves S2 (lazily filled memories, [`register_lazy`]):
//! missing-page faults there are delivered as messages, read by the handler
//! thread in [`super::lazy`].

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::OnceLock;

use rustix::mm::UserfaultfdFlags;

// From <linux/userfaultfd.h>.
const UFFD_USER_MODE_ONLY: u32 = 1;
const UFFD_API: u64 = 0xaa;
const UFFD_FEATURE_WP_HUGETLBFS_SHMEM: u64 = 1 << 12;
const UFFD_FEATURE_WP_ASYNC: u64 = 1 << 15;
const UFFDIO_REGISTER_MODE_WP: u64 = 1 << 1;
const UFFDIO_API: libc::c_ulong = 0xc018_aa3f;
const UFFDIO_REGISTER: libc::c_ulong = 0xc020_aa00;

#[repr(C)]
struct UffdioApi {
    api: u64,
    features: u64,
    ioctls: u64,
}

#[repr(C)]
struct UffdioRegister {
    start: u64,
    len: u64,
    mode: u64,
    ioctls: u64,
}

/// The process-wide tracker (opened on first use), or why it is unavailable.
fn tracker() -> Result<&'static OwnedFd, &'static str> {
    static FD: OnceLock<Result<OwnedFd, String>> = OnceLock::new();
    FD.get_or_init(|| open().map_err(|e| format!("userfaultfd write-protect tracking unavailable: {e}")))
        .as_ref()
        .map_err(String::as_str)
}

fn open() -> io::Result<OwnedFd> {
    let flags = UserfaultfdFlags::CLOEXEC
        | UserfaultfdFlags::NONBLOCK
        | UserfaultfdFlags::from_bits_retain(UFFD_USER_MODE_ONLY);
    // SAFETY: creating a userfaultfd has no effect until a range is registered.
    let fd = unsafe { rustix::mm::userfaultfd(flags)? };
    // Our memories are private mappings of memfds, which count as shmem.
    let mut api = UffdioApi {
        api: UFFD_API,
        features: UFFD_FEATURE_WP_ASYNC | UFFD_FEATURE_WP_HUGETLBFS_SHMEM,
        ioctls: 0,
    };
    // SAFETY: `api` is a valid `struct uffdio_api` that outlives the call.
    if unsafe { libc::ioctl(fd.as_raw_fd(), UFFDIO_API, &mut api) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// Whether write-protect tracking works on this kernel.
pub fn available() -> Result<(), String> {
    tracker().map(|_| ()).map_err(str::to_string)
}

/// Registers `[addr, addr + len)` for asynchronous write-protect tracking.
///
/// Pages are initially *unprotected*; the first [`super::DirtyTracking::WriteProtect`]
/// scan reports every anonymous page and protects it.
pub(super) fn register(addr: usize, len: usize) -> io::Result<()> {
    let fd = tracker().map_err(io::Error::other)?;
    let mut reg =
        UffdioRegister { start: addr as u64, len: len as u64, mode: UFFDIO_REGISTER_MODE_WP, ioctls: 0 };
    // SAFETY: `reg` is a valid `struct uffdio_register` that outlives the call.
    // Registration only changes how write faults in the range are resolved.
    if unsafe { libc::ioctl(fd.as_raw_fd(), UFFDIO_REGISTER, &mut reg) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

const UFFDIO_REGISTER_MODE_MISSING: u64 = 1 << 0;
const UFFDIO_COPY: libc::c_ulong = 0xc028_aa03;
const UFFDIO_COPY_MODE_WP: u64 = 1 << 1;
/// `uffd_msg.event` of a page fault.
const UFFD_EVENT_PAGEFAULT: u8 = 0x12;

#[repr(C)]
struct UffdioCopy {
    dst: u64,
    src: u64,
    len: u64,
    mode: u64,
    copy: i64,
}

/// Registers `[addr, addr + len)` (anonymous memory) for lazy filling (S2):
/// missing pages are reported to [`next_faults`], and filled pages are
/// tracked like [`register`]ed ones.
pub(super) fn register_lazy(addr: usize, len: usize) -> io::Result<()> {
    let fd = tracker().map_err(io::Error::other)?;
    let mode = UFFDIO_REGISTER_MODE_MISSING | UFFDIO_REGISTER_MODE_WP;
    let mut reg = UffdioRegister { start: addr as u64, len: len as u64, mode, ioctls: 0 };
    // SAFETY: `reg` is a valid `struct uffdio_register` that outlives the call.
    // From now on a user-mode access to a missing page in the range blocks
    // until `copy_page` fills it; the caller has a handler running.
    if unsafe { libc::ioctl(fd.as_raw_fd(), UFFDIO_REGISTER, &mut reg) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Blocks until missing-page faults are pending and appends their (page
/// aligned) addresses to `out`.
pub(super) fn next_faults(out: &mut Vec<usize>) -> io::Result<()> {
    let fd = tracker().map_err(io::Error::other)?;
    let mut pfd = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: `pfd` is one valid pollfd.
    if unsafe { libc::poll(&mut pfd, 1, -1) } < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::Interrupted { Ok(()) } else { Err(e) };
    }
    let mut msgs = [0u8; 32 * 64];
    // SAFETY: reading into our own buffer; the fd is non-blocking.
    let n = unsafe { libc::read(fd.as_raw_fd(), msgs.as_mut_ptr().cast(), msgs.len()) };
    if n < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::WouldBlock { Ok(()) } else { Err(e) };
    }
    // `struct uffd_msg` is 32 packed bytes: the event, then for a page fault
    // `flags` at 8 and `address` at 16.
    for msg in msgs[..n as usize].chunks_exact(32) {
        if msg[0] == UFFD_EVENT_PAGEFAULT {
            let addr = u64::from_ne_bytes(msg[16..24].try_into().unwrap()) as usize;
            out.push(addr & !(super::PAGE - 1));
        }
    }
    Ok(())
}

/// Fills the missing page at `dst` with `src`, write-protected, and wakes the
/// threads waiting on it. A page that is already present is left alone.
///
/// # Safety
/// `dst` must be a page of a range registered with [`register_lazy`].
pub(super) unsafe fn copy_page(dst: usize, src: &[u8; super::PAGE]) -> io::Result<()> {
    let fd = tracker().map_err(io::Error::other)?;
    let mut copy = UffdioCopy {
        dst: dst as u64,
        src: src.as_ptr() as u64,
        len: super::PAGE as u64,
        mode: UFFDIO_COPY_MODE_WP,
        copy: 0,
    };
    // SAFETY: `copy` is a valid `struct uffdio_copy`; `src` is a readable page;
    // `dst` is a missing page of a lazily filled memory (caller's contract),
    // so filling it is exactly what a fault there waits for.
    if unsafe { libc::ioctl(fd.as_raw_fd(), UFFDIO_COPY, &mut copy) } < 0 {
        let e = io::Error::last_os_error();
        // Already present (a duplicate fault), or the range went away.
        if matches!(e.raw_os_error(), Some(libc::EEXIST | libc::ENOENT | libc::ESRCH)) {
            return Ok(());
        }
        return Err(e);
    }
    Ok(())
}
