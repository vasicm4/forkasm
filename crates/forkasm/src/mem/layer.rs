//! Immutable page layers (sparse memfds) and the stacks snapshots are made of.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::{Arc, OnceLock};

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

/// One `mmap` of a spawn: memory `run` comes from layer `layer` (an index
/// into [`LayerStack::layers`]) at byte `file_off` of its storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Segment {
    pub(crate) run: Run,
    pub(crate) layer: usize,
    pub(crate) file_off: usize,
}

/// A base image plus layers, oldest first. Mapping them in order over a
/// memory reproduces the snapshot's contents; [`Self::plan`] gives the same
/// result with one mapping per visible segment.
#[derive(Clone)]
pub(crate) struct LayerStack {
    base: Arc<BaseImage>,
    layers: Vec<Arc<Layer>>,
    byte_size: usize,
    /// Computed on first use; shared by clones with the same layers.
    plan: Arc<OnceLock<Vec<Segment>>>,
}

impl LayerStack {
    pub(crate) fn root(base: Arc<BaseImage>, byte_size: usize) -> LayerStack {
        assert!(byte_size.is_multiple_of(PAGE) && byte_size <= base.size());
        LayerStack { base, layers: Vec::new(), byte_size, plan: Arc::default() }
    }

    /// This stack with `layer` on top, describing a memory of `byte_size`.
    pub(crate) fn push(&self, layer: Arc<Layer>, byte_size: usize) -> LayerStack {
        assert!(byte_size >= self.byte_size && layer.byte_size() <= byte_size);
        let mut layers = self.layers.clone();
        layers.push(layer);
        LayerStack { base: self.base.clone(), layers, byte_size, plan: Arc::default() }
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

    /// Every page layers `from..` hold, as sorted runs merged across
    /// `gap`-page holes, together with `extra`.
    pub(crate) fn covered_runs(&self, from: usize, extra: &[Run], gap: usize) -> Vec<Run> {
        let all =
            self.layers[from..].iter().flat_map(|l| l.runs().iter().copied()).chain(extra.iter().copied());
        super::coalesce(all.collect(), gap)
    }

    /// The bottom `keep` layers, describing a memory of `byte_size`.
    pub(crate) fn truncated(&self, keep: usize, byte_size: usize) -> LayerStack {
        assert!(byte_size >= self.byte_size);
        let layers = self.layers[..keep].to_vec();
        LayerStack { base: self.base.clone(), layers, byte_size, plan: Arc::default() }
    }

    /// How many mappings a spawn would need if layers `keep..` were replaced
    /// by one layer holding `top` (sorted, disjoint runs).
    pub(crate) fn segments_with_top(&self, keep: usize, top: &[Run]) -> usize {
        let mut runs: Vec<Vec<(Run, usize)>> =
            self.layers[..keep].iter().map(|l| l.mapped_runs().collect()).collect();
        let mut off = 0;
        runs.push(
            top.iter()
                .map(|&r| {
                    off += r.byte_len();
                    (r, off - r.byte_len())
                })
                .collect(),
        );
        paint(&runs).len()
    }

    /// The mappings a spawn makes on top of the base image: for every page
    /// some layer holds, the newest such layer, merged into maximal segments.
    pub(crate) fn plan(&self) -> &[Segment] {
        self.plan.get_or_init(|| build_plan(&self.layers))
    }

    /// Number of mappings (≈ VMAs) a spawn from this stack needs on top of
    /// the base image.
    pub(crate) fn segments(&self) -> usize {
        self.plan().len()
    }

    /// Bytes held by layers (excluding the base image).
    pub(crate) fn layer_bytes(&self) -> usize {
        self.layers.iter().map(|l| l.pages() * PAGE).sum()
    }
}

fn build_plan(layers: &[Arc<Layer>]) -> Vec<Segment> {
    let runs: Vec<Vec<(Run, usize)>> = layers.iter().map(|l| l.mapped_runs().collect()).collect();
    paint(&runs)
}

/// Paints layers (each a list of `(run, file offset)`) newest first: each run
/// contributes only the pages no newer layer covers. `covered` holds disjoint
/// `[start, end)` page intervals.
fn paint(layers: &[Vec<(Run, usize)>]) -> Vec<Segment> {
    let mut covered: BTreeMap<usize, usize> = BTreeMap::new();
    let mut segments = Vec::new();
    for (index, runs) in layers.iter().enumerate().rev() {
        for &(run, file_off) in runs {
            let (start, end) = (run.page, run.page + run.len);
            // Covered intervals touching [start, end), including ones merely
            // adjacent, so `covered` stays merged.
            let first = covered.range(..=start).next_back().filter(|&(_, &e)| e >= start).map(|(&s, _)| s);
            let touching: Vec<(usize, usize)> =
                covered.range(first.unwrap_or(start)..=end).map(|(&s, &e)| (s, e)).collect();
            let mut cursor = start;
            let mut emit = |from: usize, to: usize| {
                if from < to {
                    let run = Run { page: from, len: to - from };
                    segments.push(Segment { run, layer: index, file_off: file_off + (from - start) * PAGE });
                }
            };
            for &(s, e) in &touching {
                emit(cursor, s.min(end));
                cursor = cursor.max(e);
            }
            emit(cursor, end);
            let merged_start = touching.first().map_or(start, |&(s, _)| s.min(start));
            let merged_end = touching.last().map_or(end, |&(_, e)| e.max(end));
            for (s, _) in touching {
                covered.remove(&s);
            }
            covered.insert(merged_start, merged_end);
        }
    }
    segments.sort_by_key(|s| s.run.page);
    let mut merged: Vec<Segment> = Vec::with_capacity(segments.len());
    for seg in segments {
        if let Some(last) = merged.last_mut()
            && last.layer == seg.layer
            && last.run.page + last.run.len == seg.run.page
            && last.file_off + last.run.byte_len() == seg.file_off
        {
            last.run.len += seg.run.len;
            continue;
        }
        merged.push(seg);
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference: per page, the newest layer holding it and that page's offset.
    fn brute(layers: &[Vec<(Run, usize)>], pages: usize) -> Vec<Option<(usize, usize)>> {
        let mut owner = vec![None; pages];
        for (index, runs) in layers.iter().enumerate() {
            for &(run, off) in runs {
                for (i, o) in owner[run.page..run.page + run.len].iter_mut().enumerate() {
                    *o = Some((index, off + i * PAGE));
                }
            }
        }
        owner
    }

    fn expand(plan: &[Segment], pages: usize) -> Vec<Option<(usize, usize)>> {
        let mut owner = vec![None; pages];
        for seg in plan {
            for (i, o) in owner[seg.run.page..seg.run.page + seg.run.len].iter_mut().enumerate() {
                assert!(o.is_none(), "overlapping segments at page {}", seg.run.page + i);
                *o = Some((seg.layer, seg.file_off + i * PAGE));
            }
        }
        owner
    }

    /// Packs `runs` the way layers are stored: consecutively from offset 0.
    fn packed(runs: &[(usize, usize)]) -> Vec<(Run, usize)> {
        let mut off = 0;
        runs.iter()
            .map(|&(page, len)| {
                let r = (Run { page, len }, off);
                off += len * PAGE;
                r
            })
            .collect()
    }

    #[test]
    fn newest_layer_wins_and_segments_merge() {
        let layers = vec![packed(&[(0, 10)]), packed(&[(2, 2), (6, 1)]), packed(&[(3, 4)])];
        let plan = paint(&layers);
        assert_eq!(expand(&plan, 12), brute(&layers, 12));
        let shape: Vec<(usize, usize, usize)> =
            plan.iter().map(|s| (s.run.page, s.run.len, s.layer)).collect();
        assert_eq!(shape, [(0, 2, 0), (2, 1, 1), (3, 4, 2), (7, 3, 0)]);
    }

    #[test]
    fn random_stacks_match_brute_force() {
        let mut x: u64 = 0x1234_5678;
        let mut next = |n: usize| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % n as u64) as usize
        };
        for _ in 0..2000 {
            let pages = 64;
            let layers: Vec<Vec<(Run, usize)>> = (0..1 + next(6))
                .map(|_| {
                    let mut runs = Vec::new();
                    let mut p = next(4);
                    while p < pages {
                        let len = 1 + next(5).min(pages - p - 1);
                        runs.push((p, len));
                        p += len + 1 + next(6);
                    }
                    packed(&runs)
                })
                .collect();
            let plan = paint(&layers);
            assert_eq!(expand(&plan, pages), brute(&layers, pages));
            for w in plan.windows(2) {
                let (a, b) = (w[0], w[1]);
                let mergeable = a.layer == b.layer
                    && a.run.page + a.run.len == b.run.page
                    && a.file_off + a.run.byte_len() == b.file_off;
                assert!(!mergeable, "unmerged segments {a:?} {b:?}");
            }
        }
    }
}
