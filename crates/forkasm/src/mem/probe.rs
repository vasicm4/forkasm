//! Environment probes for `scripts/envcheck.sh` (`forkasm-cli envcheck`).
//!
//! Each probe checks a kernel behaviour the memory strategies depend on.

use std::fmt;
use std::os::fd::AsFd;

use rustix::mm::UserfaultfdFlags;

use super::mapping::write_all_at;
use super::pagemap::{self, DirtyScan, entry_is_dirty};
use super::{PAGE, Run, sys};

/// Outcome of one probe.
pub struct Probe {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

impl fmt::Display for Probe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mark = if self.ok { "ok  " } else { "FAIL" };
        write!(f, "[{mark}] {}: {}", self.name, self.detail)
    }
}

fn probe(name: &'static str, r: Result<String, String>) -> Probe {
    match r {
        Ok(detail) => Probe { name, ok: true, detail },
        Err(detail) => Probe { name, ok: false, detail },
    }
}

/// Runs every probe.
pub fn run_all() -> Vec<Probe> {
    vec![
        probe("memfd_create", memfd()),
        probe("pagemap PM_FILE on MAP_PRIVATE memfd", pm_file()),
        probe("PAGEMAP_SCAN ioctl", pagemap_scan()),
        probe("userfaultfd(UFFD_USER_MODE_ONLY)", userfaultfd()),
    ]
}

fn memfd() -> Result<String, String> {
    sys::memfd("forkasm-probe", 1 << 32).map_err(|e| e.to_string())?;
    Ok("created a sparse 4 GiB memfd".into())
}

/// A 4-page `MAP_PRIVATE` mapping of a memfd: page 0 untouched, page 1 read,
/// page 2 written, page 3 read then written.
struct Scratch {
    base: *mut u8,
}

impl Scratch {
    const PAGES: usize = 4;

    fn new() -> std::io::Result<Scratch> {
        let fd = sys::memfd("forkasm-probe", Self::PAGES * PAGE)?;
        write_all_at(fd.as_fd(), &[7u8; Self::PAGES * PAGE], 0)?;
        let scratch = Scratch { base: sys::reserve(Self::PAGES * PAGE)? };
        let base = scratch.base;
        // SAFETY: `base` is a fresh reservation we own; the file covers it.
        unsafe { sys::map_file_fixed(base, Self::PAGES * PAGE, &fd, 0, true)? };
        // SAFETY: all four pages are mapped read-write and only we use them.
        unsafe {
            std::ptr::read_volatile(base.add(PAGE));
            std::ptr::write_volatile(base.add(2 * PAGE), 1);
            std::ptr::read_volatile(base.add(3 * PAGE));
            std::ptr::write_volatile(base.add(3 * PAGE), 1);
        }
        Ok(scratch)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // SAFETY: we own the mapping and nothing references it any more.
        let _ = unsafe { sys::unmap(self.base, Self::PAGES * PAGE) };
    }
}

const EXPECTED_DIRTY: [bool; 4] = [false, false, true, true];

fn pm_file() -> Result<String, String> {
    let s = Scratch::new().map_err(|e| e.to_string())?;
    let entries = pagemap::read_entries(s.base as usize, Scratch::PAGES).map_err(|e| e.to_string())?;
    let dirty: Vec<bool> = entries.iter().map(|&e| entry_is_dirty(e)).collect();
    let flags: Vec<String> = entries.iter().map(|e| format!("{:#x}", e >> 55)).collect();
    if dirty == EXPECTED_DIRTY {
        Ok(format!("untouched/read/written/read+written = {dirty:?}; flag bits 55-63: {flags:?}"))
    } else {
        Err(format!("expected {EXPECTED_DIRTY:?}, got {dirty:?}; flag bits 55-63: {flags:?}"))
    }
}

fn pagemap_scan() -> Result<String, String> {
    let s = Scratch::new().map_err(|e| e.to_string())?;
    let runs = pagemap::dirty_runs(s.base as usize, Scratch::PAGES * PAGE, DirtyScan::PagemapScan)
        .map_err(|e| e.to_string())?;
    if runs == [Run { page: 2, len: 2 }] {
        Ok(format!("dirty runs {runs:?}"))
    } else {
        Err(format!("expected one run at page 2 of 2 pages, got {runs:?}"))
    }
}

fn userfaultfd() -> Result<String, String> {
    const UFFD_USER_MODE_ONLY: u32 = 1;
    let flags = UserfaultfdFlags::CLOEXEC | UserfaultfdFlags::from_bits_retain(UFFD_USER_MODE_ONLY);
    // SAFETY: creating a userfaultfd has no effect until a range is registered.
    match unsafe { rustix::mm::userfaultfd(flags) } {
        Ok(_) => Ok("available (user-mode faults only)".into()),
        Err(e) => Err(e.to_string()),
    }
}
