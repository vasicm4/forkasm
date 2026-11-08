//! Prototype tests: Wasmtime running on forkasm-managed memories.

use std::sync::Arc;

use wasmtime::{Config, Engine, Instance, Module, Store, TypedFunc};

use super::*;

fn engine() -> Engine {
    let mut config = Config::new();
    config.with_host_memory(Arc::new(ForkasmMemoryCreator));
    config.memory_init_cow(false);
    Engine::new(&config).unwrap()
}

const WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "load") (param i32) (result i32) (i32.load8_u (local.get 0)))
  (func (export "store") (param i32 i32) (i32.store8 (local.get 0) (local.get 1)))
  (func (export "grow") (param i32) (result i32) (memory.grow (local.get 0))))
"#;

struct Guest {
    store: Store<()>,
    load: TypedFunc<i32, i32>,
    store_fn: TypedFunc<(i32, i32), ()>,
    grow: TypedFunc<i32, i32>,
    mem: MemHandle,
}

impl Guest {
    fn new(engine: &Engine, module: &Module, init: MemInit) -> Guest {
        let mut store = Store::new(engine, ());
        let (instance, mem) = instantiate_with(init, || Ok(Instance::new(&mut store, module, &[])?)).unwrap();
        Guest {
            load: instance.get_typed_func(&mut store, "load").unwrap(),
            store_fn: instance.get_typed_func(&mut store, "store").unwrap(),
            grow: instance.get_typed_func(&mut store, "grow").unwrap(),
            store,
            mem,
        }
    }

    fn load(&mut self, addr: i32) -> i32 {
        self.load.call(&mut self.store, addr).unwrap()
    }

    fn store(&mut self, addr: i32, v: i32) {
        self.store_fn.call(&mut self.store, (addr, v)).unwrap()
    }
}

#[test]
fn zero_memory_runs_and_grows() {
    let engine = engine();
    let module = Module::new(&engine, wat::parse_str(WAT).unwrap()).unwrap();
    let mut g = Guest::new(&engine, &module, MemInit::Zero);
    assert_eq!(g.mem.len(), 64 * 1024);
    g.store(10, 5);
    assert_eq!(g.load(10), 5);
    assert_eq!(g.grow.call(&mut g.store, 2).unwrap(), 1);
    assert_eq!(g.mem.len(), 3 * 64 * 1024);
    g.store(3 * 64 * 1024 - 1, 9);
    assert_eq!(g.load(3 * 64 * 1024 - 1), 9);
    // Out of bounds still traps through Wasmtime's guard handling.
    assert!(g.load.call(&mut g.store, 3 * 64 * 1024).is_err());
}

#[test]
fn copy_init_larger_than_minimum() {
    let engine = engine();
    let module = Module::new(&engine, wat::parse_str(WAT).unwrap()).unwrap();
    let mut bytes = vec![0u8; 2 * 64 * 1024];
    bytes[70_000] = 77;
    let mut g = Guest::new(&engine, &module, MemInit::Copy(bytes.into()));
    assert_eq!(g.load(70_000), 77);
}

/// SPEC issue 1: Wasmtime re-applies active data segments after our creator
/// has initialised the memory, clobbering restored contents.
#[test]
fn data_segments_clobber_restored_memory() {
    let engine = engine();
    let wat = r#"(module (memory 1) (data (i32.const 0) "h")
        (func (export "load") (param i32) (result i32) (i32.load8_u (local.get 0)))
        (func (export "store") (param i32 i32) (i32.store8 (local.get 0) (local.get 1)))
        (func (export "grow") (param i32) (result i32) (memory.grow (local.get 0))))"#;
    let module = Module::new(&engine, wat::parse_str(wat).unwrap()).unwrap();
    let mut bytes = vec![0u8; 64 * 1024];
    bytes[0] = b'X';
    let mut g = Guest::new(&engine, &module, MemInit::Copy(bytes.into()));
    assert_eq!(g.load(0), i32::from(b'h'));
}

#[test]
fn layered_cow_spawn_and_isolation() {
    let engine = engine();
    let module = Module::new(&engine, wat::parse_str(WAT).unwrap()).unwrap();
    let reservation = 1 << 32;
    let base = Arc::new(BaseImage::new(reservation, [(100, &[42u8][..])]).unwrap());
    let root = LayerStack::root(base, 64 * 1024);

    let mut parent = Guest::new(&engine, &module, MemInit::Layers(root.clone(), false));
    assert_eq!(parent.mem.reservation(), reservation);
    assert_eq!(parent.load(100), 42);
    for scan in [DirtyTracking::Pagemap, DirtyTracking::PagemapScan] {
        assert_eq!(parent.mem.dirty_runs(scan).unwrap(), []);
    }

    parent.store(5000, 1);
    parent.store(5001, 2);
    parent.store(20_000, 3);
    for scan in [DirtyTracking::Pagemap, DirtyTracking::PagemapScan] {
        let runs = parent.mem.dirty_runs(scan).unwrap();
        assert_eq!(runs, [Run { page: 1, len: 1 }, Run { page: 4, len: 1 }], "{scan:?}");
    }

    // Snapshot: capture the dirty pages, then remap the parent over them.
    let runs = parent.mem.dirty_runs(DirtyTracking::PagemapScan).unwrap();
    let pool = LayerPool::new(PoolConfig::default()).unwrap();
    let layer = Arc::new(parent.mem.capture_layer(runs, &pool).unwrap());
    parent.mem.map_layer(&layer).unwrap();
    let snap = root.push(layer, parent.mem.len());
    assert_eq!(parent.mem.dirty_runs(DirtyTracking::PagemapScan).unwrap(), []);
    assert_eq!((parent.load(5000), parent.load(5001), parent.load(20_000)), (1, 2, 3));

    let mut a = Guest::new(&engine, &module, MemInit::Layers(snap.clone(), false));
    let mut b = Guest::new(&engine, &module, MemInit::Layers(snap.clone(), false));
    assert_eq!((a.load(100), a.load(5000), a.load(20_000)), (42, 1, 3));

    a.store(5000, 10);
    b.store(5000, 20);
    parent.store(5000, 30);
    assert_eq!((a.load(5000), b.load(5000), parent.load(5000)), (10, 20, 30));
    assert_eq!(a.mem.dirty_runs(DirtyTracking::Pagemap).unwrap(), [Run { page: 1, len: 1 }]);

    // A child can grow past the snapshot size; new pages are zero.
    assert_eq!(a.grow.call(&mut a.store, 1).unwrap(), 1);
    assert_eq!(a.load(70_000), 0);
    a.store(70_000, 8);
    assert_eq!(a.load(70_000), 8);

    // Full recompose drops every private page.
    a.mem.compose(&snap.resized(a.mem.len())).unwrap();
    assert_eq!((a.load(5000), a.load(70_000)), (1, 0));
}

#[test]
fn write_protect_tracking_reports_each_write_once() {
    if let Err(e) = write_protect_available() {
        eprintln!("skipping: {e}");
        return;
    }
    let wp = DirtyTracking::WriteProtect;
    let engine = engine();
    let module = Module::new(&engine, wat::parse_str(WAT).unwrap()).unwrap();
    let base = Arc::new(BaseImage::new(1 << 32, [(100, &[42u8][..])]).unwrap());
    let root = LayerStack::root(base, 64 * 1024);
    let mut g = Guest::new(&engine, &module, MemInit::Layers(root.clone(), true));

    // Reads fault in file pages; they are never reported.
    assert_eq!(g.load(100), 42);
    assert_eq!(g.load(30_000), 0);
    assert_eq!(g.mem.dirty_runs(wp).unwrap(), []);

    g.store(5000, 1);
    g.store(20_000, 3);
    let runs = g.mem.dirty_runs(wp).unwrap();
    assert_eq!(runs, [Run { page: 1, len: 1 }, Run { page: 4, len: 1 }]);
    // The scan re-protected them: nothing new until the next write.
    assert_eq!(g.mem.dirty_runs(wp).unwrap(), []);
    assert_eq!((g.load(5000), g.load(20_000)), (1, 3));
    assert_eq!(g.mem.dirty_runs(wp).unwrap(), []);

    // Rewriting an already-private page is caught again.
    g.store(5001, 2);
    assert_eq!(g.mem.dirty_runs(wp).unwrap(), [Run { page: 1, len: 1 }]);

    // Pages beyond the initial size, after growth, are tracked too.
    assert_eq!(g.grow.call(&mut g.store, 1).unwrap(), 1);
    g.store(70_000, 9);
    assert_eq!(g.mem.dirty_runs(wp).unwrap(), [Run { page: 17, len: 1 }]);

    // A layer captured from tracked pages spawns correctly. Scans are
    // destructive: pages reported earlier but never captured (page 17 here)
    // are lost to the next layer, so `Sandbox` captures after every scan.
    g.store(5000, 7);
    let pool = LayerPool::new(PoolConfig::default()).unwrap();
    let runs = g.mem.dirty_runs(wp).unwrap();
    let layer = Arc::new(g.mem.capture_layer(runs, &pool).unwrap());
    let snap = root.push(layer, g.mem.len());
    let mut child = Guest::new(&engine, &module, MemInit::Layers(snap, true));
    assert_eq!((child.load(5000), child.load(5001), child.load(70_000)), (7, 2, 0));
    assert_eq!(child.mem.dirty_runs(wp).unwrap(), []);
}

#[test]
fn pool_packs_layers_and_reuses_slabs() {
    let engine = engine();
    let module = Module::new(&engine, wat::parse_str(WAT).unwrap()).unwrap();
    let base = Arc::new(BaseImage::new(1 << 32, []).unwrap());
    let root = LayerStack::root(base, 64 * 1024);
    // Tiny slabs (3 pages) so layers spill into new slabs and large ones get
    // their own storage.
    let pool = LayerPool::new(PoolConfig { slab_bytes: 3 * PAGE, ready_slabs: 1 }).unwrap();
    pool.fill().unwrap();
    let mut g = Guest::new(&engine, &module, MemInit::Layers(root.clone(), false));
    let mut stack = root;
    for (round, pages) in [[1, 3].as_slice(), &[2], &[0, 5, 9, 12], &[7]].into_iter().enumerate() {
        for &p in pages {
            g.store((p * PAGE + round) as i32, (round + 1) as i32);
        }
        let runs = g.mem.dirty_runs(DirtyTracking::PagemapScan).unwrap();
        let layer = Arc::new(g.mem.capture_layer(runs, &pool).unwrap());
        g.mem.map_layer(&layer).unwrap();
        stack = stack.push(layer, g.mem.len());
    }
    let mut child = Guest::new(&engine, &module, MemInit::Layers(stack, false));
    // The child pins its layers: dropping every other reference must not
    // release (punch) pages it still maps.
    drop(g.mem.clone());
    std::thread::sleep(std::time::Duration::from_millis(20));
    for p in 0..16usize {
        for round in 0..4usize {
            assert_eq!(child.load((p * PAGE + round) as i32), g.load((p * PAGE + round) as i32), "page {p}");
        }
    }
    assert_eq!(child.load((7 * PAGE + 3) as i32), 4);
}
