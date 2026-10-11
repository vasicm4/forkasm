//! The engine, loaded guest modules, and spawning sandboxes.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use wasmtime::{Engine, InstancePre, Linker, Module};

use crate::mem::{
    BaseImage, DirtyTracking, ForkasmMemoryCreator, LayerPool, LayerStack, MemInit, PAGE, PoolConfig,
};
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
    /// S2: snapshots as S1; spawn maps nothing and fills each page from the
    /// layers on first access (userfaultfd). Needs write-protect tracking,
    /// which it always uses.
    Lazy,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub strategy: Strategy,
    /// How S1 finds dirty pages; `Auto` picks the best the kernel supports.
    pub dirty_tracking: DirtyTracking,
    /// Pre-allocated layer storage for S1; `None` allocates on each snapshot.
    pub layer_pool: Option<PoolConfig>,
    /// When S1 flattens a snapshot's layers into one (`None`: never, except
    /// through [`Sandbox::snapshot_compacted`](crate::Sandbox::snapshot_compacted)).
    pub compaction: Option<Compaction>,
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
            dirty_tracking: DirtyTracking::default(),
            layer_pool: Some(PoolConfig::default()),
            compaction: Some(Compaction::default()),
            memory_reservation: 1 << 32,
            memory_guard: 32 << 20,
            real_time: false,
        }
    }
}

/// Compaction thresholds (SPEC §5.3 step 6).
///
/// Every layer run costs a VMA in every sandbox spawned from the snapshot,
/// and spawn time grows with the number of runs. When a new snapshot would
/// exceed either limit, its layers are flattened into one: every page any
/// layer holds is copied into a single new layer, with holes of up to
/// `gap_pages` filled in so runs merge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Compaction {
    /// Most layers a snapshot may have.
    pub max_layers: usize,
    /// Most runs (≈ VMAs per spawned sandbox) a snapshot may have.
    pub max_runs: usize,
    /// Holes of at most this many pages between runs are copied too, trading
    /// copied bytes for fewer VMAs. `usize::MAX` makes one run.
    pub gap_pages: usize,
}

impl Default for Compaction {
    fn default() -> Self {
        Compaction { max_layers: 32, max_runs: 512, gap_pages: 16 }
    }
}

pub struct Runtime {
    engine: Engine,
    config: Config,
    pool: Option<Arc<LayerPool>>,
}

impl Runtime {
    pub fn new(mut config: Config) -> Result<Runtime> {
        if config.strategy == Strategy::Lazy {
            config.dirty_tracking = DirtyTracking::WriteProtect;
        }
        if config.dirty_tracking == DirtyTracking::WriteProtect {
            crate::mem::write_protect_available().map_err(anyhow::Error::msg)?;
        }
        config.dirty_tracking = config.dirty_tracking.resolve();
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
        let pool = match (config.strategy, config.layer_pool) {
            (Strategy::Layered | Strategy::Lazy, Some(pool)) => Some(pool),
            // A pool with nothing ready: every slab is allocated on demand.
            (Strategy::Layered | Strategy::Lazy, None) => {
                Some(PoolConfig { ready_slabs: 0, ..PoolConfig::default() })
            }
            (Strategy::Copy, _) => None,
        };
        let pool = pool.map(LayerPool::new).transpose()?.map(Arc::new);
        Ok(Runtime { engine: Engine::new(&wt)?, config, pool })
    }

    /// The effective configuration (`Auto` settings resolved).
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// How many times a snapshot found no pre-allocated slab ready and had to
    /// allocate one on the spot.
    pub fn layer_pool_misses(&self) -> usize {
        self.pool.as_ref().map_or(0, |p| p.misses())
    }

    /// Blocks until the layer pool has all its slabs ready, so the next
    /// snapshots don't pay for allocation (benchmarks, latency-critical forks).
    pub fn warm_up(&self) -> Result<()> {
        if let Some(pool) = &self.pool {
            pool.fill()?;
        }
        Ok(())
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
            Strategy::Layered | Strategy::Lazy => {
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
            lazy: self.config.strategy == Strategy::Lazy,
            dirty_tracking: self.config.dirty_tracking,
            compaction: self.config.compaction,
            pool: self.pool.clone(),
        })))
    }

    /// Instantiates a fresh root sandbox and runs its initialisers.
    pub fn spawn(&self, module: &GuestModule, opts: SpawnOpts) -> Result<Sandbox> {
        let mut vfs = crate::vfs::Vfs::default();
        for (guest, host) in &opts.mounts {
            vfs.mount_host(guest, host).with_context(|| format!("mounting {} at {guest}", host.display()))?;
        }
        let wasi = WasiState {
            vfs,
            args: opts.args.into(),
            env: opts.env.into(),
            clock: Clock { real: self.config.real_time, ..Clock::default() },
            rng: Rng::new(opts.seed),
            ..WasiState::default()
        };
        let init = module.0.initial.to_init(module);
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
    /// Host directories copied read-only into the root's filesystem, as
    /// `(guest path, host path)`; shared by every fork.
    pub mounts: Vec<(String, std::path::PathBuf)>,
}

/// A rewritten, compiled guest, ready to spawn. Cheap to clone.
#[derive(Clone)]
pub struct GuestModule(pub(crate) Arc<ModuleInner>);

impl GuestModule {
    pub(crate) fn tracks_writes(&self) -> bool {
        self.0.dirty_tracking == DirtyTracking::WriteProtect
    }

    /// Spawns fill memory lazily (S2).
    pub(crate) fn lazy(&self) -> bool {
        self.0.lazy
    }
}

pub(crate) struct ModuleInner {
    pub(crate) pre: InstancePre<WasiState>,
    /// The memory a root sandbox starts from.
    pub(crate) initial: MemSnap,
    pub(crate) globals: Vec<String>,
    pub(crate) has_start: bool,
    pub(crate) lazy: bool,
    pub(crate) dirty_tracking: DirtyTracking,
    pub(crate) compaction: Option<Compaction>,
    /// Layer storage (S1 only).
    pub(crate) pool: Option<Arc<LayerPool>>,
}

impl MemSnap {
    /// How a sandbox of `module` starts from this memory.
    pub(crate) fn to_init(&self, module: &GuestModule) -> MemInit {
        match self {
            MemSnap::Copy(bytes) => MemInit::Copy(bytes.clone()),
            MemSnap::Layered(stack) if module.lazy() => MemInit::Lazy(stack.clone()),
            MemSnap::Layered(stack) => MemInit::Layers(stack.clone(), module.tracks_writes()),
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
