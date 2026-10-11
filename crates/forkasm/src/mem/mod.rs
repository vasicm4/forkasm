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

#[cfg(test)]
mod tests;
