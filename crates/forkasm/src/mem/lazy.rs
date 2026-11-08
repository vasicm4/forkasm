//! S2 (SPEC §5.3): lazily filled memories.
//!
//! A spawn maps nothing: the memory is plain anonymous memory registered with
//! the process-wide userfaultfd for missing pages *and* write-protect
//! tracking. The first user-mode access to a page blocks; a single handler
//! thread looks the page up in the snapshot it was spawned from (newest layer
//! holding it, else the base image) and fills it with `UFFDIO_COPY`,
//! write-protected. So a page that was only read is not dirty, and the
//! write-protect scan of S1 finds exactly the written pages.
//!
//! Pages never touched keep their spawn-time contents forever, so the source
//! a memory was spawned from stays valid for it even after the sandbox takes
//! snapshots of its own.
//!
//! Kernel-mode accesses to a missing page fail with `EFAULT` instead of
//! faulting (`UFFD_USER_MODE_ONLY`), so the host must touch pages from user
//! mode before handing them to a syscall ([`prefault`]).

use std::collections::BTreeMap;
use std::io;
use std::sync::{Mutex, Once};

use super::layer::LayerStack;
use super::{PAGE, Run, uffd};

/// Lazily filled memories: reservation start → (reservation length, source).
static REGISTRY: Mutex<BTreeMap<usize, (usize, LayerStack)>> = Mutex::new(BTreeMap::new());

fn registry() -> std::sync::MutexGuard<'static, BTreeMap<usize, (usize, LayerStack)>> {
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner())
}

/// Makes `[base, base + len)` (anonymous, so far untouched) fill itself from
/// `source` on first access.
pub(super) fn register(base: usize, len: usize, source: &LayerStack) -> io::Result<()> {
    start_handler();
    // Compute the mapping plan now (cached per snapshot), not on first fault.
    source.plan();
    registry().insert(base, (len, source.clone()));
    if let Err(e) = uffd::register_lazy(base, len) {
        registry().remove(&base);
        return Err(e);
    }
    Ok(())
}

/// Forgets the memory at `base`; called before it is unmapped.
pub(super) fn unregister(base: usize) {
    registry().remove(&base);
}

/// Reads one byte of every page of `runs` of the memory at `base`, so they
/// are filled before a syscall reads them.
///
/// # Safety
/// `runs` must lie in the accessible part of the lazily filled memory at
/// `base`, and the sandbox must be quiescent.
pub(super) unsafe fn prefault(base: *const u8, runs: &[Run]) {
    for run in runs {
        for page in run.page..run.page + run.len {
            // SAFETY: accessible memory (caller's contract); a read changes
            // nothing.
            unsafe { std::ptr::read_volatile(base.add(page * PAGE)) };
        }
    }
}

fn start_handler() {
    static START: Once = Once::new();
    START.call_once(|| {
        std::thread::Builder::new()
            .name("forkasm-uffd".into())
            .spawn(handler)
            .expect("starting the userfaultfd handler thread");
    });
}

fn handler() {
    let mut faults = Vec::new();
    let mut page = Box::new([0u8; PAGE]);
    loop {
        faults.clear();
        if let Err(e) = uffd::next_faults(&mut faults) {
            eprintln!("forkasm: userfaultfd handler stopped: {e}");
            return;
        }
        for &addr in &faults {
            if let Err(e) = fill(addr, &mut page) {
                eprintln!("forkasm: filling page {addr:#x}: {e}");
            }
        }
    }
}

/// Fills the page at `addr` from the source of the memory containing it.
///
/// Holds the registry lock throughout, so the memory cannot be unregistered
/// (and its address reused by another memory) between the lookup and the
/// fill. Nothing that holds the lock touches lazily filled memory.
fn fill(addr: usize, page: &mut [u8; PAGE]) -> io::Result<()> {
    let reg = registry();
    let Some((&base, (len, stack))) = reg.range(..=addr).next_back() else { return Ok(()) };
    if addr >= base + len {
        // A stale fault for a memory that is gone: nobody waits on it.
        return Ok(());
    }
    let index = (addr - base) / PAGE;
    let plan = stack.plan();
    let seg = plan.partition_point(|s| s.run.page + s.run.len <= index);
    match plan.get(seg).filter(|s| s.run.page <= index) {
        Some(s) => {
            let fd = stack.layers()[s.layer].fd();
            read_exact_at(fd, page, s.file_off + (index - s.run.page) * PAGE)?;
        }
        // Not in any layer: the base image (which covers the whole
        // reservation, zeros past the module's data).
        None => read_exact_at(stack.base().fd(), page, index * PAGE)?,
    }
    // SAFETY: `addr` is a page of a memory registered with `register` and
    // still registered (we hold the lock `unregister` needs, and it runs
    // before the memory is unmapped).
    unsafe { uffd::copy_page(addr, page) }
}

fn read_exact_at(fd: std::os::fd::BorrowedFd<'_>, buf: &mut [u8; PAGE], off: usize) -> io::Result<()> {
    let mut done = 0;
    while done < PAGE {
        let n = rustix::io::pread(fd, &mut buf[done..], (off + done) as u64)?;
        if n == 0 {
            // Past the end of the file: zeros.
            buf[done..].fill(0);
            break;
        }
        done += n;
    }
    Ok(())
}
