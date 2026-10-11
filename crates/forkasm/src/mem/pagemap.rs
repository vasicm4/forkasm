//! Dirty-page detection through `/proc/self/pagemap`.
//!
//! A sandbox's memory is a `MAP_PRIVATE` mapping of layer files. A page the
//! guest has written since the last compose has been copied into an anonymous
//! page, so it is *dirty* exactly when it is present (or swapped) and not
//! file-backed.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::sync::Mutex;

use super::{PAGE, Run, push_page, push_run};

/// How to find dirty pages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DirtyScan {
    /// Read one 64-bit entry per page from `/proc/self/pagemap`.
    Pagemap,
    /// The `PAGEMAP_SCAN` ioctl (Linux ≥ 6.7): the kernel returns only the
    /// matching ranges.
    #[default]
    PagemapScan,
}

const PM_PRESENT: u64 = 1 << 63;
const PM_SWAP: u64 = 1 << 62;
const PM_FILE: u64 = 1 << 61;

/// Whether a pagemap entry describes a page written since it was mapped.
pub(super) fn entry_is_dirty(entry: u64) -> bool {
    entry & (PM_PRESENT | PM_SWAP) != 0 && entry & PM_FILE == 0
}

/// `/proc/self/pagemap`, opened once per process (re-opened after `fork`).
static PAGEMAP: Mutex<Option<(u32, File)>> = Mutex::new(None);

pub(super) fn with_pagemap<R>(f: impl FnOnce(&File) -> io::Result<R>) -> io::Result<R> {
    let pid = std::process::id();
    let mut guard = PAGEMAP.lock().unwrap_or_else(|e| e.into_inner());
    if guard.as_ref().is_none_or(|(p, _)| *p != pid) {
        *guard = Some((pid, File::open("/proc/self/pagemap")?));
    }
    f(&guard.as_ref().expect("just opened").1)
}

/// Dirty runs in `[addr, addr + len)`, as page indices relative to `addr`.
pub(super) fn dirty_runs(addr: usize, len: usize, scan: DirtyScan) -> io::Result<Vec<Run>> {
    assert!(addr.is_multiple_of(PAGE) && len.is_multiple_of(PAGE));
    match scan {
        DirtyScan::Pagemap => read_pagemap(addr, len),
        DirtyScan::PagemapScan => pagemap_scan(addr, len),
    }
}

pub(super) fn read_entries(addr: usize, pages: usize) -> io::Result<Vec<u64>> {
    let mut entries = vec![0u64; pages];
    with_pagemap(|f| {
        // SAFETY: a `u64` slice can be viewed as bytes; every bit pattern is a
        // valid `u64`.
        let bytes = unsafe { std::slice::from_raw_parts_mut(entries.as_mut_ptr().cast::<u8>(), pages * 8) };
        f.read_exact_at(bytes, (addr / PAGE * 8) as u64)
    })?;
    Ok(entries)
}

fn read_pagemap(addr: usize, len: usize) -> io::Result<Vec<Run>> {
    const CHUNK: usize = 16 * 1024;
    let mut runs = Vec::new();
    let pages = len / PAGE;
    let mut done = 0;
    while done < pages {
        let n = CHUNK.min(pages - done);
        let entries = read_entries(addr + done * PAGE, n)?;
        for (i, e) in entries.into_iter().enumerate() {
            if entry_is_dirty(e) {
                push_page(&mut runs, done + i);
            }
        }
        done += n;
    }
    Ok(runs)
}

// From <linux/fs.h>.
const PAGE_IS_FILE: u64 = 1 << 2;
const PAGE_IS_PRESENT: u64 = 1 << 3;
const PAGE_IS_SWAPPED: u64 = 1 << 4;

#[repr(C)]
#[derive(Default)]
struct PmScanArg {
    size: u64,
    flags: u64,
    start: u64,
    end: u64,
    walk_end: u64,
    vec: u64,
    vec_len: u64,
    max_pages: u64,
    category_inverted: u64,
    category_mask: u64,
    category_anyof_mask: u64,
    return_mask: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PageRegion {
    start: u64,
    end: u64,
    categories: u64,
}

/// `_IOWR('f', 16, struct pm_scan_arg)`.
const PAGEMAP_SCAN: libc::c_ulong =
    (3 << 30) | ((size_of::<PmScanArg>() as libc::c_ulong) << 16) | ((b'f' as libc::c_ulong) << 8) | 16;

fn pagemap_scan(addr: usize, len: usize) -> io::Result<Vec<Run>> {
    let mut runs = Vec::new();
    let mut regions = vec![PageRegion::default(); 256];
    let end = (addr + len) as u64;
    let mut start = addr as u64;
    with_pagemap(|f| {
        while start < end {
            let mut arg = PmScanArg {
                size: size_of::<PmScanArg>() as u64,
                start,
                end,
                vec: regions.as_mut_ptr() as u64,
                vec_len: regions.len() as u64,
                // Match pages that are (present or swapped) and not file-backed.
                category_inverted: PAGE_IS_FILE,
                category_mask: PAGE_IS_FILE,
                category_anyof_mask: PAGE_IS_PRESENT | PAGE_IS_SWAPPED,
                return_mask: PAGE_IS_FILE,
                ..Default::default()
            };
            // SAFETY: `arg` is a valid `pm_scan_arg` whose `vec` points to
            // `vec_len` writable `page_region`s that outlive the call.
            let n = unsafe { libc::ioctl(f.as_raw_fd(), PAGEMAP_SCAN, &mut arg) };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            for r in &regions[..n as usize] {
                let page = (r.start as usize - addr) / PAGE;
                let len = (r.end - r.start) as usize / PAGE;
                push_run(&mut runs, Run { page, len });
            }
            if arg.walk_end <= start {
                return Err(io::Error::other("PAGEMAP_SCAN made no progress"));
            }
            start = arg.walk_end;
        }
        Ok(())
    })?;
    Ok(runs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirty_rule() {
        assert!(!entry_is_dirty(0));
        assert!(!entry_is_dirty(PM_PRESENT | PM_FILE));
        assert!(entry_is_dirty(PM_PRESENT));
        assert!(entry_is_dirty(PM_SWAP));
        assert!(!entry_is_dirty(PM_SWAP | PM_FILE));
    }
}
