//! A live guest: store, instance, memory, globals and WASI state.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use wasmtime::{Global, Instance, Store, Val};

use crate::mem::{LayerStack, MemHandle, MemInit};
use crate::rewrite::START_EXPORT;
use crate::runtime::GuestModule;
use crate::snapshot::{MemSnap, Snapshot, SnapshotInner};
use crate::wasi::WasiState;

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
    pub memory_bytes: usize,
    /// Snapshots between this sandbox and its root.
    pub depth: usize,
    /// Layers its memory is composed from (S1).
    pub layers: usize,
    /// Mapped layer runs (≈ extra VMAs) (S1).
    pub runs: usize,
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
        if self.poisoned {
            bail!("cannot snapshot a sandbox that trapped");
        }
        let globals = self.globals.iter().map(|g| g.get(&mut self.store)).collect();
        let mem = match &self.base {
            MemSnap::Copy(_) => MemSnap::Copy(self.mem.read_all()),
            MemSnap::Layered(stack) => {
                let stack = stack.clone();
                match self.capture(&stack) {
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
    /// top of `stack`.
    fn capture(&mut self, stack: &LayerStack) -> Result<MemSnap> {
        let runs = self.mem.dirty_runs(self.module.0.dirty_tracking)?;
        let size = self.mem.len();
        if runs.is_empty() {
            return Ok(MemSnap::Layered(stack.resized(size)));
        }
        let pool = self.module.0.pool.as_deref().expect("S1 modules have a pool");
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

    pub fn stdout(&self) -> &[u8] {
        &self.store.data().stdout
    }

    /// A copy of the whole linear memory.
    pub fn memory(&self) -> Vec<u8> {
        self.mem.read_all()
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

    /// Current values of the mutable globals, in global-index order.
    pub fn globals(&mut self) -> Vec<Val> {
        self.globals.iter().map(|g| g.get(&mut self.store)).collect()
    }

    pub fn stats(&self) -> SandboxStats {
        let (layers, runs) = match &self.base {
            MemSnap::Copy(_) => (0, 0),
            MemSnap::Layered(s) => (s.layers().len(), s.runs()),
        };
        SandboxStats { memory_bytes: self.mem.len(), depth: self.depth, layers, runs }
    }
}
