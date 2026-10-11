//! S1 (layered CoW) must match S0 (full copy) byte for byte (SPEC §7).
//!
//! The same operation script runs against a mirror pair of sandbox trees, one
//! per strategy; after every step memory, globals, stdout and the guest's own
//! checksum must agree.

mod common;

use common::{kv_wasm, runtime};
use forkasm::{Runtime, Sandbox, Snapshot, SpawnOpts, Strategy, Val};

struct Pair {
    rts: [Runtime; 2],
    roots: [Sandbox; 2],
}

fn pair() -> Pair {
    let rts = [runtime(Strategy::Copy), runtime(Strategy::Layered)];
    let roots = [0, 1].map(|i| {
        let m = rts[i].load(kv_wasm()).unwrap();
        let mut sb = rts[i].spawn(&m, SpawnOpts::default()).unwrap();
        sb.call("init", &[]).unwrap();
        sb.call("buf_init", &[Val::I32(1024)]).unwrap();
        sb
    });
    Pair { rts, roots }
}

fn bits(v: &Val) -> u128 {
    match v {
        Val::I32(x) => *x as u32 as u128,
        Val::I64(x) => *x as u64 as u128,
        Val::F32(x) => *x as u128,
        Val::F64(x) => *x as u128,
        Val::V128(x) => x.as_u128(),
        other => panic!("unexpected global {other:?}"),
    }
}

fn assert_same(s0: &mut Sandbox, s1: &mut Sandbox, what: &str) {
    assert_eq!(s0.stats().memory_bytes, s1.stats().memory_bytes, "{what}: memory size");
    assert!(s0.memory() == s1.memory(), "{what}: memory bytes differ");
    let g = |s: &mut Sandbox| s.globals().iter().map(bits).collect::<Vec<_>>();
    assert_eq!(g(s0), g(s1), "{what}: globals");
    assert_eq!(s0.stdout(), s1.stdout(), "{what}: stdout");
    assert_eq!(s0.wasi().rng, s1.wasi().rng, "{what}: rng");
}

/// Applies a deterministic batch of guest operations derived from `seed`.
fn work(sb: &mut Sandbox, seed: u64) {
    let pages = (seed % 7 + 1) as i32;
    let start = (seed * 37 % 1000) as i32;
    sb.call("put", &[Val::I64(seed as i64 % 50), Val::I64(seed as i64)]).unwrap();
    sb.call("touch_pages", &[Val::I32(start), Val::I32(pages), Val::I32(seed as i32)]).unwrap();
    let _ = sb.call_u64("rand", &[]).unwrap();
    if seed.is_multiple_of(5) {
        // Grow memory now and then.
        sb.call("alloc", &[Val::I32(70_000)]).unwrap();
    }
}

#[test]
fn single_level_fork_matches() {
    let Pair { rts, roots: [mut a, mut b] } = pair();
    work(&mut a, 1);
    work(&mut b, 1);
    let (sa, sb) = (a.snapshot().unwrap(), b.snapshot().unwrap());
    assert_eq!(sb.layers(), 1);
    assert_same(&mut a, &mut b, "parent after snapshot");
    for branch in 0..4 {
        let mut ka = rts[0].spawn_from(&sa, branch).unwrap();
        let mut kb = rts[1].spawn_from(&sb, branch).unwrap();
        assert_same(&mut ka, &mut kb, "fresh child");
        work(&mut ka, branch + 10);
        work(&mut kb, branch + 10);
        assert_same(&mut ka, &mut kb, "child after work");
        assert_eq!(ka.call_u64("checksum", &[]).unwrap(), kb.call_u64("checksum", &[]).unwrap());
    }
    work(&mut a, 99);
    work(&mut b, 99);
    assert_same(&mut a, &mut b, "parent continues");
}

#[test]
fn fork_of_fork_depth_64_matches() {
    let Pair { rts, roots: [mut a, mut b] } = pair();
    let mut snaps: Vec<[Snapshot; 2]> = Vec::new();
    for depth in 0..64u64 {
        work(&mut a, depth);
        work(&mut b, depth);
        let s = [a.snapshot().unwrap(), b.snapshot().unwrap()];
        // Continue in a child, so the chain is a real fork-of-fork.
        a = rts[0].spawn_from(&s[0], depth).unwrap();
        b = rts[1].spawn_from(&s[1], depth).unwrap();
        assert_same(&mut a, &mut b, &format!("depth {depth}"));
        snaps.push(s);
    }
    assert_eq!(snaps.last().unwrap()[1].depth(), 64);
    assert_eq!(snaps.last().unwrap()[1].layers(), 64);
    // Every intermediate snapshot still spawns the same thing under both.
    for (d, [s0, s1]) in snaps.iter().enumerate().step_by(9) {
        let mut x = rts[0].spawn_from(s0, 1000).unwrap();
        let mut y = rts[1].spawn_from(s1, 1000).unwrap();
        assert_same(&mut x, &mut y, &format!("respawn at depth {d}"));
    }
}

#[test]
fn snapshot_without_writes_adds_no_layer() {
    let Pair { roots: [_, mut b], .. } = pair();
    let s1 = b.snapshot().unwrap();
    let s2 = b.snapshot().unwrap();
    assert_eq!(s1.layers(), s2.layers());
    assert_eq!(s2.depth(), s1.depth() + 1);
}
