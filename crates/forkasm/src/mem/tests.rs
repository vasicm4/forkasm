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
        let (instance, mem) =
            instantiate_with(init, || Ok(Instance::new(&mut store, module, &[])?)).unwrap();
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

    let mut parent = Guest::new(&engine, &module, MemInit::Layers(root.clone()));
    assert_eq!(parent.mem.reservation(), reservation);
    assert_eq!(parent.load(100), 42);
    for scan in [DirtyScan::Pagemap, DirtyScan::PagemapScan] {
        assert_eq!(parent.mem.dirty_runs(scan).unwrap(), []);
    }

    parent.store(5000, 1);
    parent.store(5001, 2);
    parent.store(20_000, 3);
    for scan in [DirtyScan::Pagemap, DirtyScan::PagemapScan] {
        let runs = parent.mem.dirty_runs(scan).unwrap();
        assert_eq!(runs, [Run { page: 1, len: 1 }, Run { page: 4, len: 1 }], "{scan:?}");
    }

    // Snapshot: capture the dirty pages, then remap the parent over them.
    let runs = parent.mem.dirty_runs(DirtyScan::PagemapScan).unwrap();
    let layer = Arc::new(parent.mem.capture_layer(runs).unwrap());
    parent.mem.map_layer(&layer).unwrap();
    let snap = root.push(layer, parent.mem.len());
    assert_eq!(parent.mem.dirty_runs(DirtyScan::PagemapScan).unwrap(), []);
    assert_eq!((parent.load(5000), parent.load(5001), parent.load(20_000)), (1, 2, 3));

    let mut a = Guest::new(&engine, &module, MemInit::Layers(snap.clone()));
    let mut b = Guest::new(&engine, &module, MemInit::Layers(snap.clone()));
    assert_eq!((a.load(100), a.load(5000), a.load(20_000)), (42, 1, 3));

    a.store(5000, 10);
    b.store(5000, 20);
    parent.store(5000, 30);
    assert_eq!((a.load(5000), b.load(5000), parent.load(5000)), (10, 20, 30));
    assert_eq!(a.mem.dirty_runs(DirtyScan::Pagemap).unwrap(), [Run { page: 1, len: 1 }]);

    // A child can grow past the snapshot size; new pages are zero.
    assert_eq!(a.grow.call(&mut a.store, 1).unwrap(), 1);
    assert_eq!(a.load(70_000), 0);
    a.store(70_000, 8);
    assert_eq!(a.load(70_000), 8);

    // Full recompose drops every private page.
    a.mem.compose(&snap.resized(a.mem.len())).unwrap();
    assert_eq!((a.load(5000), a.load(70_000)), (1, 0));
}
