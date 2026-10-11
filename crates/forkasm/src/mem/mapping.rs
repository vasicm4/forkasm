//! The address-space reservation behind one linear memory, and the two views
//! of it: [`Region`] (handed to Wasmtime) and [`MemHandle`] (kept by the host).

use std::io;
use std::os::fd::AsFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use wasmtime::LinearMemory;

use super::layer::{Layer, LayerStack};
use super::pagemap::{self, DirtyScan};
use super::{PAGE, Run, sys};

/// One reservation: `reservation` bytes usable by the guest followed by
/// `guard` bytes that stay `PROT_NONE`. Unmapped when the last of the
/// [`Region`] and every [`MemHandle`] are dropped.
struct Mapping {
    base: *mut u8,
    reservation: usize,
    guard: usize,
    /// Bytes currently accessible (`PROT_READ | PROT_WRITE`) from `base`.
    accessible: AtomicUsize,
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
        assert!(len <= self.reservation && len % PAGE == 0);
        assert!(stack.byte_size() <= len);
        // SAFETY: `[base, base + len)` is inside our reservation and the
        // sandbox is quiescent (no guest code holds references into it). The
        // base file is at least `reservation` bytes long, so no page we map
        // lies past its end.
        unsafe { sys::map_file_fixed(self.base, len, stack.base().fd(), 0, true)? };
        for layer in stack.layers() {
            self.map_layer(layer)?;
        }
        self.accessible.store(len, Ordering::Release);
        Ok(())
    }

    fn map_layer(&self, layer: &Layer) -> io::Result<()> {
        for run in layer.runs() {
            let end = run.byte_offset() + run.byte_len();
            assert!(end <= layer.byte_size() && end <= self.reservation);
            // SAFETY: the run lies inside our reservation and inside the layer
            // file (checked above), which is never truncated. Callers only map
            // layers no larger than the memory being composed. The sandbox is
            // quiescent.
            unsafe {
                sys::map_file_fixed(
                    self.base.add(run.byte_offset()),
                    run.byte_len(),
                    layer.fd(),
                    run.byte_offset(),
                    true,
                )?
            };
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
        let map = Arc::new(Mapping { base, reservation, guard, accessible: AtomicUsize::new(0) });
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
    /// `stack` over `[0, len)`.
    pub(super) fn init_layers(&self, stack: &LayerStack, len: usize) -> io::Result<()> {
        assert!(self.len() == 0 && stack.base().size() >= self.0.reservation);
        // SAFETY: the reservation is ours and still entirely `PROT_NONE`; the
        // base file covers all of it.
        unsafe {
            sys::map_file_fixed(self.0.base, self.0.reservation, stack.base().fd(), 0, false)?
        };
        self.0.compose(stack, len)
    }

    /// Replaces the current contents with `stack` (discarding private pages).
    pub(crate) fn compose(&self, stack: &LayerStack) -> io::Result<()> {
        self.0.compose(stack, self.len())
    }

    /// Maps `layer`'s runs over themselves, dropping the private copies of
    /// exactly those pages. Used by a parent right after it snapshots.
    pub(crate) fn map_layer(&self, layer: &Layer) -> io::Result<()> {
        self.0.map_layer(layer)
    }

    /// Copies the accessible memory out.
    pub(crate) fn read_all(&self) -> Vec<u8> {
        // SAFETY: the sandbox is quiescent (type-level contract above).
        unsafe { self.0.bytes(0, self.len()) }.to_vec()
    }

    /// Copies `data` into memory at `off`.
    pub(crate) fn write(&self, off: usize, data: &[u8]) {
        assert!(off.checked_add(data.len()).is_some_and(|end| end <= self.len()));
        // SAFETY: the range is accessible (checked above) and the sandbox is
        // quiescent; `data` cannot alias guest memory because no Rust slice
        // into guest memory outlives a `MemHandle` call.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self.0.base.add(off), data.len()) };
    }

    /// Pages written since the memory was last composed from layers.
    pub(crate) fn dirty_runs(&self, scan: DirtyScan) -> io::Result<Vec<Run>> {
        pagemap::dirty_runs(self.0.base as usize, self.len(), scan)
    }

    /// Copies `runs` of memory into a new layer file.
    pub(crate) fn capture_layer(&self, runs: Vec<Run>) -> io::Result<Layer> {
        let size = self.len();
        let fd = sys::memfd("forkasm-layer", size)?;
        for run in &runs {
            // SAFETY: the sandbox is quiescent; `bytes` bounds-checks the run.
            let src = unsafe { self.0.bytes(run.byte_offset(), run.byte_len()) };
            write_all_at(fd.as_fd(), src, run.byte_offset())?;
        }
        Ok(Layer::new(fd, runs, size))
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
