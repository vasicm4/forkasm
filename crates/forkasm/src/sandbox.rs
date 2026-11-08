//! A live guest: store, instance, memory, globals and WASI state.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use wasmtime::{Global, Instance, Store, Val};

use crate::Compaction;
use crate::mem::{LayerStack, MemHandle, MemInit, Run};
use crate::rewrite::START_EXPORT;
use crate::runtime::GuestModule;
use crate::snapshot::{MemSnap, Snapshot, SnapshotInner};
use crate::wasi::WasiState;

/// A live guest instance: its store, memory, globals and WASI state.
///
/// Drive it with [`call`](Self::call); between calls, capture it with
/// [`snapshot`](Self::snapshot) and spawn children from the snapshot.
/// Dropping a sandbox frees its private pages. A sandbox whose call trapped is
/// *poisoned*: it can no longer be called or snapshotted.
pub struct Sandbox {
    store: Store<WasiState>,
    instance: Instance,
    mem: MemHandle,
    globals: Vec<Global>,
    module: GuestModule,
    /// The snapshot memory is currently composed from (S1), or the last one
    /// taken (S0, unused).
    base: MemSnap,
    depth: usize,
    /// Set when a call trapped: the guest may have been stopped mid-update, so
    /// it is no longer at a quiescent point and must not be snapshotted.
    poisoned: bool,
}

/// Point-in-time numbers for tests and benchmarks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxStats {
    /// Current size of the linear memory.
    pub memory_bytes: usize,
    /// Snapshots between this sandbox and its root.
    pub depth: usize,
    /// Layers its memory is composed from (S1).
    pub layers: usize,
    /// Layer segments its memory maps over the base image (≈ extra VMAs) (S1).
    pub runs: usize,
    /// Files and directories in its filesystem.
    pub inodes: usize,
    /// Open file descriptors, including stdio and the preopened root.
    pub open_fds: usize,
}

impl Sandbox {
    pub(crate) fn instantiate(
        module: GuestModule,
        init: MemInit,
        wasi: WasiState,
        parent: Option<&Snapshot>,
    ) -> Result<Sandbox> {
        let mut store = Store::new(module.0.pre.module().engine(), wasi);
        let (instance, mem) =
            crate::mem::instantiate_with(init, || Ok(module.0.pre.instantiate(&mut store)?))?;
        let globals = module
            .0
            .globals
            .iter()
            .map(|name| instance.get_global(&mut store, name).context("missing global export"))
            .collect::<Result<Vec<_>>>()?;
        let (base, depth) = match parent {
            Some(snap) => {
                for (g, v) in globals.iter().zip(&snap.0.globals) {
                    g.set(&mut store, *v)?;
                }
                (snap.0.mem.clone(), snap.0.depth)
            }
            None => (module.0.initial.clone(), 0),
        };
        Ok(Sandbox { store, instance, mem, globals, module, base, depth, poisoned: false })
    }

    /// Runs the former `start` function and a reactor's `_initialize`.
    pub(crate) fn run_initialisers(&mut self) -> Result<()> {
        if self.module.0.has_start {
            self.call(START_EXPORT, &[])?;
        }
        if self.instance.get_func(&mut self.store, "_initialize").is_some() {
            self.call("_initialize", &[])?;
        }
        Ok(())
    }

    /// Calls an exported function. A trap poisons the sandbox.
    pub fn call(&mut self, name: &str, args: &[Val]) -> Result<Vec<Val>> {
        if self.poisoned {
            bail!("sandbox trapped earlier and can no longer be used");
        }
        if let Some(code) = self.store.data().exit {
            bail!("sandbox exited with code {code}");
        }
        let func = self
            .instance
            .get_func(&mut self.store, name)
            .with_context(|| format!("no exported function {name:?}"))?;
        let mut results = vec![Val::I32(0); func.ty(&self.store).results().len()];
        if let Err(e) = func.call(&mut self.store, args, &mut results) {
            self.poisoned = true;
            return Err(e.into());
        }
        Ok(results)
    }

    /// Calls `name` and returns its single result as a `u64` (i32 results are
    /// zero-extended). Convenience for tests.
    pub fn call_u64(&mut self, name: &str, args: &[Val]) -> Result<u64> {
        match self.call(name, args)?[..] {
            [Val::I64(v)] => Ok(v as u64),
            [Val::I32(v)] => Ok(v as u32 as u64),
            ref r => bail!("{name} returned {r:?}, expected one integer"),
        }
    }

    /// Captures the sandbox. The sandbox itself continues on top of the
    /// snapshot (SPEC §3): with S1 its dirty pages move into a new shared
    /// layer and it gets fresh private copies only when it writes again.
    pub fn snapshot(&mut self) -> Result<Snapshot> {
        self.snapshot_inner(false)
    }

    /// Like [`snapshot`](Self::snapshot), but always flattens S1 layers into
    /// one (see [`Compaction`](crate::Compaction)); same as `snapshot` for S0.
    pub fn snapshot_compacted(&mut self) -> Result<Snapshot> {
        self.snapshot_inner(true)
    }

    fn snapshot_inner(&mut self, force_compact: bool) -> Result<Snapshot> {
        if self.poisoned {
            bail!("cannot snapshot a sandbox that trapped");
        }
        let globals = self.globals.iter().map(|g| g.get(&mut self.store)).collect();
        let mem = match &self.base {
            MemSnap::Copy(_) => MemSnap::Copy(self.mem.read_all()),
            MemSnap::Layered(stack) => {
                let stack = stack.clone();
                match self.capture(&stack, force_compact) {
                    Ok(snap) => snap,
                    Err(e) => {
                        // A failure after the dirty scan may leave pages
                        // neither captured nor still marked dirty (write-protect
                        // scans are destructive), so the next snapshot could
                        // silently miss them.
                        self.poisoned = true;
                        return Err(e.context("snapshot failed; the sandbox can no longer be used"));
                    }
                }
            }
        };
        self.base = mem.clone();
        self.depth += 1;
        Ok(Snapshot(Arc::new(SnapshotInner {
            module: self.module.clone(),
            mem,
            globals,
            wasi: self.store.data().clone(),
            depth: self.depth,
        })))
    }

    /// S1: captures the pages dirtied since the last snapshot into a layer on
    /// top of `stack`, or flattens everything into one layer when compaction
    /// is due.
    fn capture(&mut self, stack: &LayerStack, force_compact: bool) -> Result<MemSnap> {
        let runs = self.mem.dirty_runs(self.module.0.dirty_tracking)?;
        let size = self.mem.len();
        let pool = self.module.0.pool.as_deref().expect("S1 modules have a pool");
        let compaction = self.module.0.compaction;
        // Compaction is due when the new stack would have too many layers or
        // need too many mappings per spawn. (Checked before capturing, using
        // an upper bound for the new layer's segments.)
        let due = compaction.is_some_and(|c| {
            stack.layers().len() + usize::from(!runs.is_empty()) > c.max_layers
                || stack.segments() + runs.len() > c.max_runs
        });
        if force_compact || due {
            // Merge the newest layers (and the dirty set) into one. The
            // memory currently holds exactly the snapshot's contents, so the
            // merged layer copies every page those layers cover from it.
            let c = compaction.unwrap_or_default();
            let keep = if force_compact { 0 } else { merge_point(stack, &runs, &c) };
            let covered = stack.covered_runs(keep, &runs, c.gap_pages);
            let lower = stack.truncated(keep, size);
            let merged = if covered.is_empty() {
                lower
            } else {
                let layer = Arc::new(self.mem.capture_layer(covered, pool)?);
                lower.push(layer, size)
            };
            // Re-map ourselves too: drops private pages, the merged layers'
            // mappings (and so their VMAs), and resets write tracking.
            self.mem.compose(&merged)?;
            return Ok(MemSnap::Layered(merged));
        }
        if runs.is_empty() {
            return Ok(MemSnap::Layered(stack.resized(size)));
        }
        let layer = Arc::new(self.mem.capture_layer(runs, pool)?);
        // Without write-protect tracking, the captured pages would be reported
        // again next time: swap our private copies for the shared layer. With
        // it, the scan already re-protected them and we keep them.
        if !self.module.tracks_writes() {
            self.mem.map_layer(&layer)?;
        }
        Ok(MemSnap::Layered(stack.push(layer, size)))
    }

    /// The WASI state (stdout, clock, RNG, …).
    pub fn wasi(&self) -> &WasiState {
        self.store.data()
    }

    /// Everything the guest wrote to stdout.
    pub fn stdout(&self) -> Vec<u8> {
        self.store.data().stdout.to_vec()
    }

    /// Everything the guest wrote to stderr.
    pub fn stderr(&self) -> Vec<u8> {
        self.store.data().stderr.to_vec()
    }

    /// A copy of the whole linear memory.
    pub fn memory(&self) -> Vec<u8> {
        self.mem.read_all()
    }

    /// The whole linear memory, borrowed (no copy).
    pub fn memory_view(&self) -> &[u8] {
        self.mem.view()
    }

    /// Writes `data` into linear memory at `offset`.
    pub fn write_memory(&mut self, offset: usize, data: &[u8]) -> Result<()> {
        anyhow::ensure!(
            offset.checked_add(data.len()).is_some_and(|end| end <= self.mem.len()),
            "write out of bounds"
        );
        self.mem.write(offset, data);
        Ok(())
    }

    /// Reads the whole file at absolute `path` in the sandbox's filesystem.
    pub fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        let vfs = &self.store.data().vfs;
        let ino = vfs.resolve(crate::vfs::ROOT, path).with_context(|| format!("reading {path}"))?;
        let mut buf = vec![0; vfs.inode(ino)?.size() as usize];
        vfs.read(ino, 0, &mut buf).with_context(|| format!("reading {path}"))?;
        Ok(buf)
    }

    /// Creates or replaces the file at absolute `path` (its directory must
    /// exist).
    pub fn write_file(&mut self, path: &str, data: &[u8]) -> Result<()> {
        let wasi = self.store.data_mut();
        let now = wasi.clock.peek_realtime();
        let vfs = &mut wasi.vfs;
        let (dir, name) =
            vfs.resolve_parent(crate::vfs::ROOT, path).with_context(|| format!("writing {path}"))?;
        let ino = match vfs.lookup(dir, name)? {
            Some(ino) => ino,
            None => vfs.create_file(dir, name, now)?,
        };
        vfs.truncate(ino, 0, now)?;
        vfs.write(ino, 0, data, now)?;
        Ok(())
    }

    /// Creates the directory at absolute `path` and any missing parents.
    pub fn create_dir_all(&mut self, path: &str) -> Result<()> {
        let wasi = self.store.data_mut();
        let now = wasi.clock.peek_realtime();
        wasi.vfs.create_dir_all(path, now).with_context(|| format!("creating {path}"))?;
        Ok(())
    }

    /// Current values of the mutable globals, in global-index order.
    pub fn globals(&mut self) -> Vec<Val> {
        self.globals.iter().map(|g| g.get(&mut self.store)).collect()
    }

    /// Baseline B: forks the whole host process instead of the sandbox. The
    /// child process calls `name` (which must return one integer) on its
    /// copy of this sandbox and exits; this sandbox is untouched.
    ///
    /// For benchmarks. Only the calling thread survives in the child, so the
    /// guest call must not depend on other threads (forkasm's own helper
    /// threads are not needed to run guest code).
    pub fn process_fork_call(&mut self, name: &str, args: &[Val]) -> Result<crate::mem::ProcessFork> {
        if self.poisoned {
            bail!("sandbox trapped earlier and can no longer be used");
        }
        // The child would not inherit the userfaultfd registration, so pages
        // never filled would read as zeros there.
        if self.mem.is_lazy() {
            bail!("process fork of a lazily filled (S2) sandbox is not supported");
        }
        Ok(crate::mem::fork_run(|| self.call_u64(name, args).unwrap_or(u64::MAX))?)
    }

    /// Current size, depth, layering and filesystem numbers.
    pub fn stats(&self) -> SandboxStats {
        let (layers, runs) = match &self.base {
            MemSnap::Copy(_) => (0, 0),
            MemSnap::Layered(s) => (s.layers().len(), s.segments()),
        };
        let wasi = self.store.data();
        SandboxStats {
            memory_bytes: self.mem.len(),
            depth: self.depth,
            layers,
            runs,
            inodes: wasi.vfs.inode_count(),
            open_fds: wasi.fds.len(),
        }
    }
}

/// Tiered compaction: how many of `stack`'s layers to keep when merging the
/// rest with the new dirty `runs` into one layer.
///
/// Merges the fewest newest layers such that the layer below the merged one
/// is at least [`TIER_RATIO`] times its size (so layer sizes grow
/// geometrically towards the bottom and each page is copied O(log n) times
/// over a long chain, instead of on every full flatten) and the resulting
/// stack is within half the thresholds (so the next merge is some snapshots
/// away). Falls back to flattening everything.
fn merge_point(stack: &LayerStack, runs: &[Run], c: &Compaction) -> usize {
    let layers = stack.layers();
    let mut merged: usize = runs.iter().map(|r| r.len).sum();
    for keep in (0..=layers.len()).rev() {
        if keep < layers.len() {
            merged += layers[keep].pages();
        }
        let below = if keep == 0 { usize::MAX } else { layers[keep - 1].pages() };
        if below / TIER_RATIO < merged || keep + 1 > c.max_layers / 2 {
            continue;
        }
        let top = stack.covered_runs(keep, runs, c.gap_pages);
        if stack.segments_with_top(keep, &top) <= c.max_runs / 2 {
            return keep;
        }
    }
    0
}

/// Size ratio between adjacent layers that tiered compaction aims for.
const TIER_RATIO: usize = 2;
