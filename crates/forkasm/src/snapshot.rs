//! Immutable, shareable captures of a sandbox at a quiescent point.

use std::sync::Arc;

use anyhow::Result;
use wasmtime::Val;

use crate::mem::LayerStack;
use crate::runtime::GuestModule;
use crate::sandbox::Sandbox;
use crate::wasi::WasiState;

/// A snapshot's memory.
#[derive(Clone)]
pub(crate) enum MemSnap {
    /// S0: a full copy.
    Copy(Arc<[u8]>),
    /// S1: base image plus layers.
    Layered(LayerStack),
}

/// A captured sandbox. Cheap to clone; children can be spawned from it any
/// number of times.
#[derive(Clone)]
pub struct Snapshot(pub(crate) Arc<SnapshotInner>);

pub(crate) struct SnapshotInner {
    pub(crate) module: GuestModule,
    pub(crate) mem: MemSnap,
    pub(crate) globals: Vec<Val>,
    pub(crate) wasi: WasiState,
    pub(crate) depth: usize,
}

impl Snapshot {
    pub(crate) fn spawn(&self, branch: u64) -> Result<Sandbox> {
        let s = &self.0;
        let mut wasi = s.wasi.clone();
        wasi.rng = wasi.rng.fork(branch);
        Sandbox::instantiate(s.module.clone(), s.mem.to_init(s.module.tracks_writes()), wasi, Some(self))
    }

    /// Number of snapshots between this one and its root sandbox.
    pub fn depth(&self) -> usize {
        self.0.depth
    }

    /// Number of layers (S1) above the base image; 0 for S0.
    pub fn layers(&self) -> usize {
        match &self.0.mem {
            MemSnap::Copy(_) => 0,
            MemSnap::Layered(stack) => stack.layers().len(),
        }
    }

    /// Bytes of memory state this snapshot holds on its own account: the full
    /// copy for S0, the layer pages for S1 (shared with ancestors and
    /// descendants).
    pub fn bytes(&self) -> usize {
        match &self.0.mem {
            MemSnap::Copy(b) => b.len(),
            MemSnap::Layered(stack) => stack.layer_bytes(),
        }
    }

    /// The captured WASI state.
    pub fn wasi(&self) -> &WasiState {
        &self.0.wasi
    }

    /// Size of the captured linear memory.
    pub fn memory_size(&self) -> usize {
        match &self.0.mem {
            MemSnap::Copy(b) => b.len(),
            MemSnap::Layered(stack) => stack.byte_size(),
        }
    }
}
