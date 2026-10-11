//! Memory management for sandbox linear memories.
//!
//! All `unsafe` code in forkasm lives in this module. Everything exported from
//! here is a safe API; every `unsafe` block carries a `// SAFETY:` comment.

mod creator;
mod layer;
mod mapping;
mod pagemap;
mod pool;
pub mod probe;
mod sys;
mod uffd;

pub(crate) use creator::{ForkasmMemoryCreator, MemInit, instantiate_with};
pub(crate) use layer::{BaseImage, LayerStack};
pub(crate) use mapping::MemHandle;
pub use pagemap::DirtyTracking;
pub(crate) use pool::LayerPool;
pub use pool::PoolConfig;
pub use uffd::available as write_protect_available;

/// Host page size. forkasm only supports 4 KiB pages (x86_64 / aarch64 default).
pub const PAGE: usize = 4096;

/// A run of contiguous pages: `[page, page + len)`, both in host pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    pub page: usize,
    pub len: usize,
}

impl Run {
    pub fn byte_offset(&self) -> usize {
        self.page * PAGE
    }

    pub fn byte_len(&self) -> usize {
        self.len * PAGE
    }
}

/// Appends page `page` to `runs`, extending the last run when adjacent.
pub(crate) fn push_page(runs: &mut Vec<Run>, page: usize) {
    push_run(runs, Run { page, len: 1 });
}

/// Appends `run` to `runs`, merging with the last run when adjacent.
pub(crate) fn push_run(runs: &mut Vec<Run>, run: Run) {
    if let Some(last) = runs.last_mut()
        && last.page + last.len == run.page
    {
        last.len += run.len;
        return;
    }
    runs.push(run);
}

/// Sorts and merges `runs`, also joining runs separated by at most `gap`
/// pages (the gap pages are then included).
pub(crate) fn coalesce(mut runs: Vec<Run>, gap: usize) -> Vec<Run> {
    runs.sort_by_key(|r| r.page);
    let mut out: Vec<Run> = Vec::with_capacity(runs.len());
    for run in runs {
        if let Some(last) = out.last_mut()
            && run.page <= last.page + last.len + gap
        {
            last.len = last.len.max(run.page + run.len - last.page);
            continue;
        }
        out.push(run);
    }
    out
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod coalesce_tests {
    use super::*;

    fn r(page: usize, len: usize) -> Run {
        Run { page, len }
    }

    #[test]
    fn coalesce_merges_overlaps_and_small_gaps() {
        let runs = vec![r(10, 2), r(0, 3), r(2, 4), r(12, 1), r(20, 1), r(23, 2)];
        assert_eq!(coalesce(runs.clone(), 0), [r(0, 6), r(10, 3), r(20, 1), r(23, 2)]);
        assert_eq!(coalesce(runs.clone(), 2), [r(0, 6), r(10, 3), r(20, 5)]);
        assert_eq!(coalesce(runs, 100), [r(0, 25)]);
    }
}
