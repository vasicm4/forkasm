//! The address-space reservation behind one linear memory, and the two views
//! of it: [`Region`] (handed to Wasmtime) and [`MemHandle`] (kept by the host).

use std::io;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use wasmtime::LinearMemory;

use super::layer::{Layer, LayerStack};
use super::pagemap::{self, DirtyTracking};
use super::pool::LayerPool;
use super::{PAGE, Run, lazy, sys, uffd};

/// One reservation: `reservation` bytes usable by the guest followed by
/// `guard` bytes that stay `PROT_NONE`. Unmapped when the last of the
/// [`Region`] and every [`MemHandle`] are dropped.
struct Mapping {
    base: *mut u8,
    reservation: usize,
    guard: usize,
    /// Bytes currently accessible (`PROT_READ | PROT_WRITE`) from `base`.
    accessible: AtomicUsize,
    /// Registered for userfaultfd write tracking.
    tracked: AtomicBool,
    /// Filled lazily on first access (S2, [`super::lazy`]).
    lazy: AtomicBool,
    /// Every layer currently mapped. Layer pages live in shared slabs that are
    /// hole-punched when the layer is dropped, so the mapping must keep them
    /// alive (unlike a plain file, which the mapping itself would keep open).
    pinned: Mutex<Vec<Arc<Layer>>>,
}

// SAFETY: `Mapping` only stores the address of a mapping it owns. Access to the
// bytes behind it is coordinated by the sandbox: the guest touches them only
// while the store is borrowed mutably, and the host only through `MemHandle`,
// which requires the sandbox (and so the store) to be borrowed as well.
unsafe impl Send for Mapping {}
// SAFETY: see `Send` above.
unsafe impl Sync for Mapping {}

impl Drop for Mapping {
    fn drop(&mut self) {
        if *self.lazy.get_mut() {
            lazy::unregister(self.base as usize);
        }
        // SAFETY: we own the whole reservation, and the last reference to it
        // (Wasmtime's `Region` and every `MemHandle`) is gone.
        let _ = unsafe { sys::unmap(self.base, self.reservation + self.guard) };
    }
}

impl Mapping {
    fn accessible(&self) -> usize {
        self.accessible.load(Ordering::Acquire)
    }

    /// Maps the whole of `stack` over `[0, len)` and makes it accessible.
    fn compose(&self, stack: &LayerStack, len: usize) -> io::Result<()> {
        assert!(len <= self.reservation && len.is_multiple_of(PAGE));
        assert!(stack.byte_size() <= len);
        // SAFETY: `[base, base + len)` is inside our reservation and the
        // sandbox is quiescent (no guest code holds references into it). The
        // base file is at least `reservation` bytes long, so no page we map
        // lies past its end.
        unsafe { sys::map_file_fixed(self.base, len, stack.base().fd(), 0, true)? };
        let layers = stack.layers();
        *self.pinned() = layers.to_vec();
        for seg in stack.plan() {
            let layer = &layers[seg.layer];
            let end = seg.run.byte_offset() + seg.run.byte_len();
            assert!(end <= layer.byte_size() && end <= len);
            // SAFETY: the segment lies inside our reservation and inside the
            // layer's storage (checked above; the plan only takes pages from
            // a layer's own runs), which is never truncated while the layer
            // lives — and we pin it. The sandbox is quiescent.
            unsafe {
                sys::map_file_fixed(
                    self.base.add(seg.run.byte_offset()),
                    seg.run.byte_len(),
                    layer.fd(),
                    seg.file_off,
                    true,
                )?
            };
        }
        self.accessible.store(len, Ordering::Release);
        self.register_tracking()
    }

    fn pinned(&self) -> std::sync::MutexGuard<'_, Vec<Arc<Layer>>> {
        self.pinned.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn map_layer(&self, layer: &Arc<Layer>) -> io::Result<()> {
        self.pinned().push(layer.clone());
        for (run, file_off) in layer.mapped_runs() {
            let end = run.byte_offset() + run.byte_len();
            assert!(end <= layer.byte_size() && end <= self.reservation);
            // SAFETY: the run lies inside our reservation (checked above) and
            // its pages inside the layer's storage, which is never truncated
            // while the layer lives. Callers only map layers no larger than the
            // memory being composed. The sandbox is quiescent.
            unsafe {
                sys::map_file_fixed(
                    self.base.add(run.byte_offset()),
                    run.byte_len(),
                    layer.fd(),
                    file_off,
                    true,
                )?
            };
        }
        Ok(())
    }

    /// Registers the whole reservation for write tracking, if enabled. Needed
    /// again after every remap, since a new mapping starts unregistered.
    fn register_tracking(&self) -> io::Result<()> {
        if self.tracked.load(Ordering::Acquire) {
            uffd::register(self.base as usize, self.reservation)?;
        }
        Ok(())
    }

    /// Returns `[off, off + len)` of the accessible memory as a slice.
    ///
    /// # Safety
    /// The caller must ensure no guest code runs while the slice is alive.
    unsafe fn bytes(&self, off: usize, len: usize) -> &[u8] {
        assert!(off.checked_add(len).is_some_and(|end| end <= self.accessible()));
        // SAFETY: the range is accessible (checked above); exclusivity is the
        // caller's contract.
        unsafe { std::slice::from_raw_parts(self.base.add(off), len) }
    }
}

/// The [`LinearMemory`] Wasmtime owns.
pub(super) struct Region {
    map: Arc<Mapping>,
    maximum: Option<usize>,
}

impl Region {
    /// Reserves `reservation + guard` bytes, all `PROT_NONE`.
    pub(super) fn reserve(
        reservation: usize,
        guard: usize,
        maximum: Option<usize>,
    ) -> io::Result<(Region, MemHandle)> {
        let base = sys::reserve(reservation + guard)?;
        let map = Arc::new(Mapping {
            base,
            reservation,
            guard,
            accessible: AtomicUsize::new(0),
            tracked: AtomicBool::new(false),
            lazy: AtomicBool::new(false),
            pinned: Mutex::new(Vec::new()),
        });
        Ok((Region { map: map.clone(), maximum }, MemHandle(map)))
    }
}

// SAFETY: `as_ptr` is stable for the region's lifetime, `byte_capacity` bytes
// are reserved behind it followed by the guard Wasmtime asked for, and
// `byte_size` bytes from `as_ptr` are always readable and writable.
unsafe impl LinearMemory for Region {
    fn byte_size(&self) -> usize {
        self.map.accessible()
    }

    fn byte_capacity(&self) -> usize {
        self.map.reservation
    }

    fn grow_to(&mut self, new_size: usize) -> wasmtime::Result<()> {
        let old = self.map.accessible();
        if new_size > self.map.reservation || self.maximum.is_some_and(|m| new_size > m) {
            wasmtime::bail!("cannot grow memory to {new_size} bytes");
        }
        if new_size > old {
            // SAFETY: `[old, new_size)` is inside our reservation; it is
            // currently `PROT_NONE`, so nothing can reference it.
            unsafe { sys::protect_rw(self.map.base.add(old), new_size - old)? };
            self.map.accessible.store(new_size, Ordering::Release);
        }
        Ok(())
    }

    fn as_ptr(&self) -> *mut u8 {
        self.map.base
    }
}

/// The host's handle to a sandbox's linear memory.
///
/// Every method assumes the sandbox is quiescent: it must only be called while
/// the owning `Store` is not executing guest code. `Sandbox` guarantees this by
/// requiring `&mut self` for every memory operation.
#[derive(Clone)]
pub(crate) struct MemHandle(Arc<Mapping>);

impl MemHandle {
    pub(crate) fn len(&self) -> usize {
        self.0.accessible()
    }

    #[cfg(test)]
    pub(crate) fn reservation(&self) -> usize {
        self.0.reservation
    }

    /// Makes `[0, len)` accessible as zeroed anonymous memory.
    pub(super) fn init_zero(&self, len: usize) -> io::Result<()> {
        assert!(len <= self.0.reservation && self.len() == 0);
        // SAFETY: the reservation is ours and still entirely `PROT_NONE`, so
        // nothing references it.
        unsafe { sys::protect_rw(self.0.base, len)? };
        self.0.accessible.store(len, Ordering::Release);
        Ok(())
    }

    /// Maps the base image over the whole reservation (inaccessible), then
    /// `stack` over `[0, len)`. With `track_writes`, the memory is registered
    /// for [`DirtyTracking::WriteProtect`].
    pub(super) fn init_layers(&self, stack: &LayerStack, len: usize, track_writes: bool) -> io::Result<()> {
        assert!(self.len() == 0 && stack.base().size() >= self.0.reservation);
        self.0.tracked.store(track_writes, Ordering::Release);
        // SAFETY: the reservation is ours and still entirely `PROT_NONE`; the
        // base file covers all of it.
        unsafe { sys::map_file_fixed(self.0.base, self.0.reservation, stack.base().fd(), 0, false)? };
        self.0.compose(stack, len)
    }

    /// Makes `[0, len)` accessible, filling every page of the reservation
    /// from `stack` on first access (S2). Tracks writes like
    /// [`DirtyTracking::WriteProtect`].
    pub(super) fn init_lazy(&self, stack: &LayerStack, len: usize) -> io::Result<()> {
        assert!(self.len() == 0 && stack.base().size() >= self.0.reservation && stack.byte_size() <= len);
        // SAFETY: the reservation is ours and still entirely `PROT_NONE`, so
        // nothing references it.
        unsafe { sys::protect_rw(self.0.base, len)? };
        self.0.accessible.store(len, Ordering::Release);
        self.0.lazy.store(true, Ordering::Release);
        lazy::register(self.0.base as usize, self.0.reservation, stack)
    }

    /// Whether this memory is filled lazily (S2).
    pub(crate) fn is_lazy(&self) -> bool {
        self.0.lazy.load(Ordering::Acquire)
    }

    /// Replaces the current contents with `stack` (discarding private pages).
    ///
    /// A lazily filled memory is left as it is: compaction only calls this
    /// with a stack holding exactly the current contents, and its write
    /// tracking was already reset by the scan before the capture.
    pub(crate) fn compose(&self, stack: &LayerStack) -> io::Result<()> {
        if self.is_lazy() {
            return Ok(());
        }
        self.0.compose(stack, self.len())
    }

    /// Maps `layer`'s runs over themselves, dropping the private copies of
    /// exactly those pages. Used by a parent right after it snapshots.
    pub(crate) fn map_layer(&self, layer: &Arc<Layer>) -> io::Result<()> {
        self.0.map_layer(layer)
    }

    /// The accessible memory, borrowed.
    ///
    /// The slice must not outlive the caller's borrow of the owning sandbox:
    /// `Sandbox` hands it out from `&self`, so no guest code, remap or host
    /// write (all of which need `&mut Sandbox`) can happen while it lives.
    pub(crate) fn view(&self) -> &[u8] {
        // SAFETY: quiescent for the lifetime of the borrow, as documented above.
        unsafe { self.0.bytes(0, self.len()) }
    }

    /// Copies the accessible memory out.
    pub(crate) fn read_all<B: for<'a> From<&'a [u8]>>(&self) -> B {
        // SAFETY: the sandbox is quiescent (type-level contract above).
        B::from(unsafe { self.0.bytes(0, self.len()) })
    }

    /// Copies `data` into fresh anonymous memory at offset 0, pre-faulting
    /// the pages first (S0 spawn).
    pub(super) fn fill(&self, data: &[u8]) -> io::Result<()> {
        assert!(data.len() <= self.len());
        let len = data.len().next_multiple_of(PAGE);
        // SAFETY: `[base, base + len)` is accessible (`len` ≤ the page-aligned
        // accessible size) and the sandbox is still being created.
        unsafe { sys::populate_write(self.0.base, len)? };
        self.write(0, data);
        Ok(())
    }

    /// Copies `data` into memory at `off`.
    pub(crate) fn write(&self, off: usize, data: &[u8]) {
        assert!(off.checked_add(data.len()).is_some_and(|end| end <= self.len()));
        // SAFETY: the range is accessible (checked above) and the sandbox is
        // quiescent; `data` cannot alias guest memory because no Rust slice
        // into guest memory outlives a `MemHandle` call.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self.0.base.add(off), data.len()) };
    }

    /// Pages written since the last snapshot: since the memory was last
    /// composed from layers, or (with `WriteProtect`) since the last scan.
    pub(crate) fn dirty_runs(&self, tracking: DirtyTracking) -> io::Result<Vec<Run>> {
        pagemap::dirty_runs(self.0.base as usize, self.len(), tracking)
    }

    /// Copies `runs` of memory into a new layer.
    pub(crate) fn capture_layer(&self, runs: Vec<Run>, pool: &LayerPool) -> io::Result<Layer> {
        if self.is_lazy() {
            // Large layers are written with `pwritev`, whose kernel-mode
            // reads would fail on pages never filled (compaction copies
            // clean pages too).
            // SAFETY: the runs are inside the accessible memory (`bytes`
            // checks them below too) and the sandbox is quiescent.
            unsafe { lazy::prefault(self.0.base, &runs) };
        }
        // SAFETY: the sandbox is quiescent and stays so while the slices live
        // (we hold `&self` and the sandbox is borrowed by the caller); `bytes`
        // bounds-checks every run.
        let chunks: Vec<&[u8]> =
            runs.iter().map(|r| unsafe { self.0.bytes(r.byte_offset(), r.byte_len()) }).collect();
        let storage = pool.store(&chunks)?;
        Ok(Layer::new(storage, runs, self.len()))
    }
}

pub(super) fn write_all_at(fd: impl AsFd, mut buf: &[u8], mut off: usize) -> io::Result<()> {
    while !buf.is_empty() {
        let n = rustix::io::pwrite(&fd, buf, off as u64)?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        buf = &buf[n..];
        off += n;
    }
    Ok(())
}
