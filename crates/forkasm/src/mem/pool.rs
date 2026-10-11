//! Pre-allocated storage for layers.
//!
//! Allocating shmem pages costs ~1 µs each, which made it the largest part of
//! a snapshot. Instead, a background thread keeps a few *slabs* ready:
//! memfds whose pages were allocated in advance with `fallocate` and mapped
//! shared with page tables pre-filled. A snapshot carves its layer out of the
//! current slab with a bump pointer and `memcpy`s its dirty pages there
//! *packed* (a layer's file offsets differ from its memory offsets), so taking
//! a snapshot only copies.
//!
//! A dropped layer's pages are released by punching a hole in its slab on the
//! background thread; the slab itself is closed when its last layer goes.
//! Layers larger than a slab get a memfd of their own.

use std::io::{self, IoSlice};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, Weak};

use rustix::fs::{FallocateFlags, fallocate};

use super::PAGE;
use super::sys::{self, SharedMap};

/// Layer pool sizing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolConfig {
    /// Size of each pre-allocated slab. Layers larger than this get their own
    /// memfd (allocated on the snapshot path).
    pub slab_bytes: usize,
    /// Slabs kept allocated and ready, besides the one in use.
    pub ready_slabs: usize,
}

impl Default for PoolConfig {
    fn default() -> Self {
        PoolConfig { slab_bytes: 16 << 20, ready_slabs: 2 }
    }
}

/// A memfd holding the packed pages of one or more layers.
pub(crate) struct Slab {
    fd: OwnedFd,
    /// Shared mapping used to fill it (pre-allocated slabs only).
    map: Option<SharedMap>,
    worker: Sender<Job>,
}

/// A slab prepared by the background thread.
struct Prepared {
    fd: OwnedFd,
    map: SharedMap,
}

/// Where a layer's pages live.
pub(crate) struct Storage {
    slab: Arc<Slab>,
    /// Byte offset of the layer's first page in the slab.
    pub(crate) offset: usize,
    pub(crate) len: usize,
}

impl Storage {
    pub(crate) fn fd(&self) -> BorrowedFd<'_> {
        self.slab.fd.as_fd()
    }
}

impl Drop for Storage {
    fn drop(&mut self) {
        // Other layers still use the slab: give our pages back. Otherwise the
        // slab closes with us and the kernel frees everything.
        if Arc::strong_count(&self.slab) > 1 && self.len > 0 {
            let _ = self.slab.worker.send(Job::Punch(self.slab.clone(), self.offset, self.len));
        }
    }
}

enum Job {
    /// Top the ready list up to `ready_slabs`.
    Refill,
    /// Release `[offset, offset + len)` of a slab.
    Punch(Arc<Slab>, usize, usize),
}

struct Shared {
    config: PoolConfig,
    /// Allocated slabs not yet handed out.
    ready: Mutex<Vec<Prepared>>,
}

/// The slab currently being carved up, and how much of it is used.
struct Current {
    slab: Arc<Slab>,
    used: usize,
}

pub(crate) struct LayerPool {
    shared: Arc<Shared>,
    current: Mutex<Option<Current>>,
    worker: Sender<Job>,
    /// Slabs a snapshot had to create itself because none was ready.
    misses: AtomicUsize,
}

impl LayerPool {
    /// Creates the pool and starts filling it in the background.
    pub(crate) fn new(config: PoolConfig) -> io::Result<LayerPool> {
        assert!(config.slab_bytes.is_multiple_of(PAGE) && config.slab_bytes > 0);
        let shared = Arc::new(Shared { config, ready: Mutex::new(Vec::new()) });
        let (worker, jobs) = channel();
        let weak = Arc::downgrade(&shared);
        std::thread::Builder::new().name("forkasm-layer-pool".into()).spawn(move || work(weak, jobs))?;
        let _ = worker.send(Job::Refill);
        Ok(LayerPool { shared, current: Mutex::new(None), worker, misses: AtomicUsize::new(0) })
    }

    /// Blocks until the ready list is full (for benchmarks and tests).
    pub(crate) fn fill(&self) -> io::Result<()> {
        while self.lock_ready().len() < self.shared.config.ready_slabs {
            let slab = prepare_slab(self.shared.config.slab_bytes)?;
            self.lock_ready().push(slab);
        }
        Ok(())
    }

    pub(crate) fn misses(&self) -> usize {
        self.misses.load(Ordering::Relaxed)
    }

    fn lock_ready(&self) -> std::sync::MutexGuard<'_, Vec<Prepared>> {
        self.shared.ready.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Stores `chunks` (each a whole number of pages) contiguously and
    /// returns where.
    pub(crate) fn store(&self, chunks: &[&[u8]]) -> io::Result<Storage> {
        let len: usize = chunks.iter().map(|c| c.len()).sum();
        debug_assert!(len.is_multiple_of(PAGE));
        let storage = if len > self.shared.config.slab_bytes {
            // Too large for a slab: a dedicated, exactly-sized memfd.
            let fd = sys::memfd("forkasm-layer", len)?;
            Storage { slab: Arc::new(Slab { fd, map: None, worker: self.worker.clone() }), offset: 0, len }
        } else {
            self.carve(len)?
        };
        match &storage.slab.map {
            Some(map) => {
                let mut at = storage.offset;
                for c in chunks {
                    map.write(at, c);
                    at += c.len();
                }
            }
            None => write_chunks(storage.fd(), chunks, storage.offset)?,
        }
        Ok(storage)
    }

    /// Reserves `len` bytes of the current slab, moving to a fresh one when it
    /// is full.
    fn carve(&self, len: usize) -> io::Result<Storage> {
        let mut current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        let slab_bytes = self.shared.config.slab_bytes;
        if current.as_ref().is_none_or(|c| c.used + len > slab_bytes) {
            if let Some(old) = current.take()
                && old.used < slab_bytes
            {
                // Release the unused, pre-allocated tail.
                let _ = self.worker.send(Job::Punch(old.slab, old.used, slab_bytes - old.used));
            }
            let (fd, map) = match self.lock_ready().pop() {
                Some(p) => (p.fd, Some(p.map)),
                // Pool exhausted: the pages get allocated as we write them.
                None => {
                    self.misses.fetch_add(1, Ordering::Relaxed);
                    (sys::memfd("forkasm-slab", slab_bytes)?, None)
                }
            };
            let _ = self.worker.send(Job::Refill);
            *current =
                Some(Current { slab: Arc::new(Slab { fd, map, worker: self.worker.clone() }), used: 0 });
        }
        let c = current.as_mut().expect("set above");
        let storage = Storage { slab: c.slab.clone(), offset: c.used, len };
        c.used += len;
        Ok(storage)
    }
}

fn prepare_slab(size: usize) -> io::Result<Prepared> {
    let fd = sys::memfd("forkasm-slab", size)?;
    fallocate(&fd, FallocateFlags::empty(), 0, size as u64)?;
    let map = SharedMap::new(&fd, size, true)?;
    Ok(Prepared { fd, map })
}

fn work(shared: Weak<Shared>, jobs: Receiver<Job>) {
    while let Ok(job) = jobs.recv() {
        match job {
            Job::Refill => {
                let Some(shared) = shared.upgrade() else { continue };
                loop {
                    let missing = {
                        let ready = shared.ready.lock().unwrap_or_else(|e| e.into_inner());
                        shared.config.ready_slabs.saturating_sub(ready.len())
                    };
                    if missing == 0 {
                        break;
                    }
                    match prepare_slab(shared.config.slab_bytes) {
                        Ok(slab) => shared.ready.lock().unwrap_or_else(|e| e.into_inner()).push(slab),
                        Err(_) => break, // Out of memory: snapshots fall back to allocating.
                    }
                }
            }
            Job::Punch(slab, offset, len) => {
                let flags = FallocateFlags::PUNCH_HOLE | FallocateFlags::KEEP_SIZE;
                let _ = fallocate(&slab.fd, flags, offset as u64, len as u64);
            }
        }
    }
}

/// `pwritev` of `chunks` to `fd` at `offset`, in batches of `IOV_MAX`.
fn write_chunks(fd: BorrowedFd<'_>, chunks: &[&[u8]], mut offset: usize) -> io::Result<()> {
    const IOV_MAX: usize = 1024;
    for batch in chunks.chunks(IOV_MAX) {
        let total: usize = batch.iter().map(|c| c.len()).sum();
        let iovs: Vec<IoSlice<'_>> = batch.iter().map(|c| IoSlice::new(c)).collect();
        let n = rustix::io::pwritev(fd, &iovs, offset as u64)?;
        if n < total {
            // Short write (rare): finish chunk by chunk.
            let (mut at, mut skip) = (offset, n);
            for c in batch {
                let done = skip.min(c.len());
                skip -= done;
                super::mapping::write_all_at(fd, &c[done..], at + done)?;
                at += c.len();
            }
        }
        offset += total;
    }
    Ok(())
}
