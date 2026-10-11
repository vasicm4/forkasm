//! Immutable page layers (sparse memfds) and the stacks snapshots are made of.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::Arc;

use super::mapping::write_all_at;
use super::pool::Storage;
use super::{PAGE, Run, sys};

/// The bottom layer: the module's initial memory image, sized to cover the
/// whole reservation so any page of the memory can be mapped from it.
pub(crate) struct BaseImage {
    fd: OwnedFd,
    size: usize,
}

impl BaseImage {
    /// Builds a sparse memfd of `size` bytes holding `segments` (offset, bytes).
    pub(crate) fn new<'a>(
        size: usize,
        segments: impl IntoIterator<Item = (usize, &'a [u8])>,
    ) -> io::Result<BaseImage> {
        let fd = sys::memfd("forkasm-base", size)?;
        for (off, bytes) in segments {
            write_all_at(fd.as_fd(), bytes, off)?;
        }
        Ok(BaseImage { fd, size })
    }

    pub(crate) fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    pub(crate) fn size(&self) -> usize {
        self.size
    }
}

/// The pages a sandbox dirtied between two snapshots, stored packed (in run
/// order) in pool storage.
pub(crate) struct Layer {
    storage: Storage,
    runs: Vec<Run>,
    /// Memory size when the layer was captured.
    byte_size: usize,
}

impl Layer {
    pub(super) fn new(storage: Storage, runs: Vec<Run>, byte_size: usize) -> Layer {
        debug_assert_eq!(storage.len, runs.iter().map(Run::byte_len).sum::<usize>());
        Layer { storage, runs, byte_size }
    }

    pub(crate) fn fd(&self) -> BorrowedFd<'_> {
        self.storage.fd()
    }

    pub(crate) fn runs(&self) -> &[Run] {
        &self.runs
    }

    /// Each run with the byte offset of its pages in [`Self::fd`].
    pub(crate) fn mapped_runs(&self) -> impl Iterator<Item = (Run, usize)> + '_ {
        self.runs.iter().scan(self.storage.offset, |off, &run| {
            let at = *off;
            *off += run.byte_len();
            Some((run, at))
        })
    }

    pub(crate) fn byte_size(&self) -> usize {
        self.byte_size
    }

    pub(crate) fn pages(&self) -> usize {
        self.runs.iter().map(|r| r.len).sum()
    }
}

/// A base image plus layers, oldest first. Mapping them in order over a
/// memory reproduces the snapshot's contents.
#[derive(Clone)]
pub(crate) struct LayerStack {
    base: Arc<BaseImage>,
    layers: Vec<Arc<Layer>>,
    byte_size: usize,
}

impl LayerStack {
    pub(crate) fn root(base: Arc<BaseImage>, byte_size: usize) -> LayerStack {
        assert!(byte_size.is_multiple_of(PAGE) && byte_size <= base.size());
        LayerStack { base, layers: Vec::new(), byte_size }
    }

    /// This stack with `layer` on top, describing a memory of `byte_size`.
    pub(crate) fn push(&self, layer: Arc<Layer>, byte_size: usize) -> LayerStack {
        assert!(byte_size >= self.byte_size && layer.byte_size() <= byte_size);
        let mut layers = self.layers.clone();
        layers.push(layer);
        LayerStack { base: self.base.clone(), layers, byte_size }
    }

    /// This stack describing a larger memory (the guest grew, nothing dirty).
    pub(crate) fn resized(&self, byte_size: usize) -> LayerStack {
        assert!(byte_size >= self.byte_size);
        LayerStack { byte_size, ..self.clone() }
    }

    pub(crate) fn base(&self) -> &BaseImage {
        &self.base
    }

    pub(crate) fn layers(&self) -> &[Arc<Layer>] {
        &self.layers
    }

    pub(crate) fn byte_size(&self) -> usize {
        self.byte_size
    }

    /// Number of mapped runs a spawn from this stack needs (≈ VMAs).
    pub(crate) fn runs(&self) -> usize {
        self.layers.iter().map(|l| l.runs().len()).sum()
    }

    /// Bytes held by layers (excluding the base image).
    pub(crate) fn layer_bytes(&self) -> usize {
        self.layers.iter().map(|l| l.pages() * PAGE).sum()
    }
}
