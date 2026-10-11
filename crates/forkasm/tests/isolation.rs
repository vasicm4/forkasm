//! Fork isolation for memory, globals and WASI state, under every strategy
//! (SPEC §7).

mod common;

use common::{STRATEGIES, kv_wasm, runtime};
use forkasm::{Sandbox, SpawnOpts, Strategy, Val};

fn kv(strategy: Strategy) -> (forkasm::Runtime, Sandbox) {
    let rt = runtime(strategy);
    let module = rt.load(kv_wasm()).unwrap();
    let mut root = rt.spawn(&module, SpawnOpts::default()).unwrap();
    root.call("init", &[]).unwrap();
    (rt, root)
}

fn put(sb: &mut Sandbox, k: i64, v: i64) {
    sb.call("put", &[Val::I64(k), Val::I64(v)]).unwrap();
}

fn get(sb: &mut Sandbox, k: i64) -> u64 {
    sb.call_u64("get", &[Val::I64(k)]).unwrap()
}

/// Copies `s` into guest memory and prints it through `say`.
fn say(sb: &mut Sandbox, s: &str) {
    let ptr = sb.call_u64("alloc", &[Val::I32(s.len() as i32)]).unwrap();
    sb.write_memory(ptr as usize, s.as_bytes()).unwrap();
    sb.call("say", &[Val::I32(ptr as i32), Val::I32(s.len() as i32)]).unwrap();
}

#[test]
fn root_runs_with_args_and_env() {
    for strategy in STRATEGIES {
        let rt = runtime(strategy);
        let module = rt.load(kv_wasm()).unwrap();
        let opts = SpawnOpts {
            args: vec!["kv".into(), "x".into()],
            env: vec![("A".into(), "1".into())],
            ..SpawnOpts::default()
        };
        let mut root = rt.spawn(&module, opts).unwrap();
        root.call("init", &[]).unwrap();
        assert_eq!(String::from_utf8_lossy(root.stdout()), "kv: ready (args=2, env=1)\n");
    }
}

#[test]
fn memory_isolation() {
    for strategy in STRATEGIES {
        let (rt, mut root) = kv(strategy);
        put(&mut root, 1, 10);
        let snap = root.snapshot().unwrap();
        let mut kids = rt.spawn_many(&snap, 2).unwrap();
        put(&mut kids[0], 1, 100);
        put(&mut kids[1], 1, 200);
        put(&mut kids[1], 2, 2);
        put(&mut root, 1, 300);
        assert_eq!(get(&mut kids[0], 1), 100, "{strategy:?}");
        assert_eq!(get(&mut kids[1], 1), 200);
        assert_eq!(get(&mut root, 1), 300);
        assert_eq!(get(&mut kids[0], 2), u64::MAX);
        assert_eq!(get(&mut root, 2), u64::MAX);
        // A late child still sees the snapshot, not anyone's later writes.
        let mut late = rt.spawn_from(&snap, 7).unwrap();
        assert_eq!(get(&mut late, 1), 10);
        drop(kids);
        assert_eq!(get(&mut late, 1), 10);
    }
}

#[test]
fn growth_in_child_only() {
    for strategy in STRATEGIES {
        let (rt, mut root) = kv(strategy);
        let size = root.stats().memory_bytes;
        let snap = root.snapshot().unwrap();
        let mut kid = rt.spawn_from(&snap, 0).unwrap();
        kid.call("buf_init", &[Val::I32(4096)]).unwrap(); // 16 MiB
        assert_eq!(kid.call_u64("touch_pages", &[Val::I32(0), Val::I32(4096), Val::I32(1)]).unwrap(), 4096);
        assert!(kid.stats().memory_bytes > size + (15 << 20), "{} vs {size}", kid.stats().memory_bytes);
        assert_eq!(root.stats().memory_bytes, size);
        let grown = kid.snapshot().unwrap();
        let mut grandkid = rt.spawn_from(&grown, 0).unwrap();
        assert_eq!(grandkid.call_u64("checksum", &[]).unwrap(), kid.call_u64("checksum", &[]).unwrap());
        assert_eq!(grandkid.memory(), kid.memory(), "{strategy:?}");
    }
}

#[test]
fn wasi_state_isolation_and_determinism() {
    for strategy in STRATEGIES {
        let (rt, mut root) = kv(strategy);
        say(&mut root, "before");
        let snap = root.snapshot().unwrap();
        let mut a = rt.spawn_from(&snap, 0).unwrap();
        let mut a2 = rt.spawn_from(&snap, 0).unwrap();
        let mut b = rt.spawn_from(&snap, 1).unwrap();

        // Same branch replays identically; different branches differ.
        let (ra, ra2, rb) = (
            a.call_u64("rand", &[]).unwrap(),
            a2.call_u64("rand", &[]).unwrap(),
            b.call_u64("rand", &[]).unwrap(),
        );
        assert_eq!(ra, ra2);
        assert_ne!(ra, rb);
        assert_eq!(a.call_u64("now", &[]).unwrap(), a2.call_u64("now", &[]).unwrap());

        // The parent's RNG and clock are untouched by its children.
        assert_eq!(root.wasi().rng, snap.wasi().rng);
        assert_eq!(root.wasi().clock.now_ns, snap.wasi().clock.now_ns);

        // stdout: children inherit the prefix, then diverge.
        say(&mut a, "child a");
        say(&mut root, "parent");
        assert_eq!(String::from_utf8_lossy(a.stdout()).lines().last(), Some("child a"));
        assert_eq!(String::from_utf8_lossy(root.stdout()).lines().last(), Some("parent"));
        assert!(String::from_utf8_lossy(b.stdout()).ends_with("before\n"));
    }
}

#[test]
fn globals_isolation() {
    let wat = r#"(module (memory 1)
        (global $n (mut i64) (i64.const 0))
        (func (export "inc") (result i64)
            (global.set $n (i64.add (global.get $n) (i64.const 1))) (global.get $n)))"#;
    for strategy in STRATEGIES {
        let rt = runtime(strategy);
        let module = rt.load_bytes(&wat::parse_str(wat).unwrap()).unwrap();
        let mut root = rt.spawn(&module, SpawnOpts::default()).unwrap();
        assert_eq!(root.call_u64("inc", &[]).unwrap(), 1);
        let snap = root.snapshot().unwrap();
        let mut kid = rt.spawn_from(&snap, 0).unwrap();
        assert_eq!(kid.call_u64("inc", &[]).unwrap(), 2);
        assert_eq!(kid.call_u64("inc", &[]).unwrap(), 3);
        assert_eq!(root.call_u64("inc", &[]).unwrap(), 2);
        assert_eq!(rt.spawn_from(&snap, 0).unwrap().call_u64("inc", &[]).unwrap(), 2);
        assert_eq!(root.globals()[0].i64(), Some(2));
    }
}

#[test]
fn trap_poisons_sandbox() {
    let wat = r#"(module (memory 1) (func (export "boom") unreachable) (func (export "ok")))"#;
    for strategy in STRATEGIES {
        let rt = runtime(strategy);
        let module = rt.load_bytes(&wat::parse_str(wat).unwrap()).unwrap();
        let mut sb = rt.spawn(&module, SpawnOpts::default()).unwrap();
        assert!(sb.call("boom", &[]).is_err());
        assert!(sb.call("ok", &[]).is_err());
        assert!(sb.snapshot().is_err());
    }
}

#[test]
fn children_survive_dropping_ancestors() {
    for config in common::layered_configs()
        .into_iter()
        .chain([forkasm::Config { strategy: Strategy::Copy, ..Default::default() }])
    {
        let rt = forkasm::Runtime::new(config.clone()).unwrap();
        let module = rt.load(kv_wasm()).unwrap();
        let mut root = rt.spawn(&module, SpawnOpts::default()).unwrap();
        root.call("init", &[]).unwrap();
        let mut kid = None;
        for level in 0..5 {
            put(&mut root, level, level * 10);
            let snap = root.snapshot().unwrap();
            // Siblings come and go; their layers share slabs with ours.
            for b in 0..3 {
                let mut s = rt.spawn_from(&snap, b).unwrap();
                put(&mut s, 99, 99);
                drop(s.snapshot().unwrap());
            }
            kid = Some(rt.spawn_from(&snap, 0).unwrap());
        }
        let mut kid = kid.unwrap();
        let expected = kid.memory();
        drop(root);
        // Let the pool's background thread release the dropped layers.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(kid.memory() == expected, "{:?}: memory changed", config.dirty_tracking);
        for level in 0..5 {
            assert_eq!(get(&mut kid, level), (level * 10) as u64);
        }
    }
}
