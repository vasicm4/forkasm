//! The engine, loaded guest modules, and spawning sandboxes.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use wasmtime::{Engine, InstancePre, Linker, Module};

use crate::mem::{BaseImage, DirtyScan, ForkasmMemoryCreator, LayerStack, MemInit, PAGE};
use crate::rewrite::{self, MemImage};
use crate::sandbox::Sandbox;
use crate::snapshot::{MemSnap, Snapshot};
use crate::wasi::{self, Clock, Rng, WasiState};

/// How sandbox memory is snapshotted and forked (SPEC §5.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Strategy {
    /// S0: snapshot copies the whole memory; spawn copies it back. The oracle.
    Copy,
    /// S1: snapshot captures dirty pages into a layer; spawn maps layers
    /// copy-on-write.
    #[default]
    Layered,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub strategy: Strategy,
    pub dirty_scan: DirtyScan,
    /// Bytes of address space reserved per memory (≥ the largest memory).
    pub memory_reservation: u64,
    /// Unmapped bytes after each reservation, letting Wasmtime elide bounds
    /// checks.
    pub memory_guard: u64,
    /// Give guests the host clock instead of the deterministic virtual one.
    pub real_time: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            strategy: Strategy::default(),
            dirty_scan: DirtyScan::default(),
            memory_reservation: 1 << 32,
            memory_guard: 32 << 20,
            real_time: false,
        }
    }
}

pub struct Runtime {
    engine: Engine,
    config: Config,
}

impl Runtime {
    pub fn new(config: Config) -> Result<Runtime> {
        anyhow::ensure!(
            config.memory_reservation.is_multiple_of(PAGE as u64)
                && config.memory_guard.is_multiple_of(PAGE as u64),
            "memory reservation and guard must be page multiples"
        );
        let mut wt = wasmtime::Config::new();
        wt.with_host_memory(Arc::new(ForkasmMemoryCreator))
            // Required with host memory: Wasmtime would CoW-map its own image
            // into it (and data segments are stripped anyway).
            .memory_init_cow(false)
            .memory_reservation(config.memory_reservation)
            .memory_guard_size(config.memory_guard)
            .memory_reservation_for_growth(0)
            .wasm_multi_memory(false)
            .wasm_memory64(false);
        Ok(Runtime { engine: Engine::new(&wt)?, config })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn load(&self, path: impl AsRef<Path>) -> Result<GuestModule> {
        let path = path.as_ref();
        let wasm = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        self.load_bytes(&wasm)
    }

    /// Rewrites (SPEC §5.1) and compiles a module.
    pub fn load_bytes(&self, wasm: &[u8]) -> Result<GuestModule> {
        let rw = rewrite::rewrite(wasm)?;
        let module = Module::new(&self.engine, &rw.wasm)?;
        let mut linker = Linker::new(&self.engine);
        wasi::add_to_linker(&mut linker, &module)?;
        let pre = linker.instantiate_pre(&module)?;
        let initial = match self.config.strategy {
            Strategy::Copy => MemSnap::Copy(image_bytes(&rw.image).into()),
            Strategy::Layered => {
                let size = self.config.memory_reservation as usize;
                let segments = rw.image.segments.iter().map(|(off, b)| (*off as usize, &b[..]));
                let base = Arc::new(BaseImage::new(size, segments)?);
                MemSnap::Layered(LayerStack::root(base, rw.image.min_bytes as usize))
            }
        };
        Ok(GuestModule(Arc::new(ModuleInner {
            pre,
            initial,
            globals: rw.globals,
            has_start: rw.has_start,
            dirty_scan: self.config.dirty_scan,
        })))
    }

    /// Instantiates a fresh root sandbox and runs its initialisers.
    pub fn spawn(&self, module: &GuestModule, opts: SpawnOpts) -> Result<Sandbox> {
        let wasi = WasiState {
            args: opts.args.into(),
            env: opts.env.into(),
            clock: Clock { real: self.config.real_time, ..Clock::default() },
            rng: Rng::new(opts.seed),
            ..WasiState::default()
        };
        let init = module.0.initial.to_init();
        let mut sb = Sandbox::instantiate(module.clone(), init, wasi, None)?;
        sb.run_initialisers()?;
        Ok(sb)
    }

    /// Spawns one child of `snap`. `branch` seeds the child's RNG, so the same
    /// (snapshot, branch) pair always replays identically.
    pub fn spawn_from(&self, snap: &Snapshot, branch: u64) -> Result<Sandbox> {
        snap.spawn(branch)
    }

    /// Spawns `n` children of `snap` with branches `0..n`.
    pub fn spawn_many(&self, snap: &Snapshot, n: usize) -> Result<Vec<Sandbox>> {
        (0..n as u64).map(|b| snap.spawn(b)).collect()
    }
}

/// Options for a root sandbox.
#[derive(Clone, Debug, Default)]
pub struct SpawnOpts {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// RNG seed of the root.
    pub seed: u64,
}

/// A rewritten, compiled guest, ready to spawn. Cheap to clone.
#[derive(Clone)]
pub struct GuestModule(pub(crate) Arc<ModuleInner>);

pub(crate) struct ModuleInner {
    pub(crate) pre: InstancePre<WasiState>,
    /// The memory a root sandbox starts from.
    pub(crate) initial: MemSnap,
    pub(crate) globals: Vec<String>,
    pub(crate) has_start: bool,
    pub(crate) dirty_scan: DirtyScan,
}

impl MemSnap {
    pub(crate) fn to_init(&self) -> MemInit {
        match self {
            MemSnap::Copy(bytes) => MemInit::Copy(bytes.clone()),
            MemSnap::Layered(stack) => MemInit::Layers(stack.clone()),
        }
    }
}

fn image_bytes(image: &MemImage) -> Vec<u8> {
    let mut bytes = vec![0; image.min_bytes as usize];
    for (off, data) in &image.segments {
        bytes[*off as usize..*off as usize + data.len()].copy_from_slice(data);
    }
    bytes
}
