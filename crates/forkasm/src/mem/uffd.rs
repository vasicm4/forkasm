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
