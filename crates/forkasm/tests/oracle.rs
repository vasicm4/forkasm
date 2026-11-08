//! S1 (layered CoW) must match S0 (full copy) byte for byte (SPEC §7).
//!
//! The same operation script runs against a mirror pair of sandbox trees, one
//! per strategy; after every step memory, globals, stdout and the guest's own
//! checksum must agree.

mod common;

use common::{assert_same, kv_wasm, layered_configs, runtime, work};

const BUF: i32 = 1024;
use forkasm::{Config, Runtime, Sandbox, Snapshot, SpawnOpts, Strategy, Val};

struct Pair {
    rts: [Runtime; 2],
    roots: [Sandbox; 2],
}

/// S0 next to the S1 variant `config`.
fn pair(config: &Config) -> Pair {
    let rts = [runtime(Strategy::Copy), Runtime::new(config.clone()).unwrap()];
    let roots = [0, 1].map(|i| {
        let m = rts[i].load(kv_wasm()).unwrap();
        let mut sb = rts[i].spawn(&m, SpawnOpts::default()).unwrap();
        sb.call("init", &[]).unwrap();
        sb.call("buf_init", &[Val::I32(BUF)]).unwrap();
        sb
    });
    Pair { rts, roots }
}

#[test]
fn single_level_fork_matches() {
    for config in layered_configs() {
        single_level(&config);
    }
}

fn single_level(config: &Config) {
    let Pair { rts, roots: [mut a, mut b] } = pair(config);
    work(&mut a, BUF, 1);
    work(&mut b, BUF, 1);
    let (sa, sb) = (a.snapshot().unwrap(), b.snapshot().unwrap());
    assert_eq!(sb.layers(), 1);
    assert_same(&mut a, &mut b, "parent after snapshot");
    for branch in 0..4 {
        let mut ka = rts[0].spawn_from(&sa, branch).unwrap();
        let mut kb = rts[1].spawn_from(&sb, branch).unwrap();
        assert_same(&mut ka, &mut kb, "fresh child");
        work(&mut ka, BUF, branch + 10);
        work(&mut kb, BUF, branch + 10);
        assert_same(&mut ka, &mut kb, "child after work");
        assert_eq!(ka.call_u64("checksum", &[]).unwrap(), kb.call_u64("checksum", &[]).unwrap());
    }
    work(&mut a, BUF, 99);
    work(&mut b, BUF, 99);
    assert_same(&mut a, &mut b, "parent continues");
}

#[test]
fn fork_of_fork_depth_64_matches() {
    for config in layered_configs() {
        depth_64(&config);
    }
}

/// Also snapshots the same sandbox repeatedly (a chain without children),
/// which is where write-protect tracking differs most from the remap.
fn depth_64(config: &Config) {
    let Pair { rts, roots: [mut a, mut b] } = pair(config);
    let mut snaps: Vec<[Snapshot; 2]> = Vec::new();
    for depth in 0..64u64 {
        work(&mut a, BUF, depth);
        work(&mut b, BUF, depth);
        // Now and then force a compaction regardless of thresholds.
        let s = if depth % 17 == 16 {
            [a.snapshot_compacted().unwrap(), b.snapshot_compacted().unwrap()]
        } else {
            [a.snapshot().unwrap(), b.snapshot().unwrap()]
        };
        // Mostly continue in a child, so the chain is a real fork-of-fork;
        // every third level the parent itself continues.
        if depth % 3 != 0 {
            a = rts[0].spawn_from(&s[0], depth).unwrap();
            b = rts[1].spawn_from(&s[1], depth).unwrap();
        }
        assert_same(&mut a, &mut b, &format!("{:?} depth {depth}", config.dirty_tracking));
        snaps.push(s);
    }
    assert_eq!(snaps.last().unwrap()[1].depth(), 64);
    let max_layers = config.compaction.map_or(usize::MAX, |c| c.max_layers);
    assert!(snaps.iter().all(|[_, s]| s.layers() <= max_layers));
    // Every intermediate snapshot still spawns the same thing under both.
    for (d, [s0, s1]) in snaps.iter().enumerate().step_by(9) {
        let mut x = rts[0].spawn_from(s0, 1000).unwrap();
        let mut y = rts[1].spawn_from(s1, 1000).unwrap();
        assert_same(&mut x, &mut y, &format!("respawn at depth {d}"));
    }
}

#[test]
fn snapshot_without_writes_adds_no_layer() {
    for config in layered_configs() {
        let Pair { roots: [_, mut b], .. } = pair(&config);
        no_writes(&mut b);
    }
}

fn no_writes(b: &mut Sandbox) {
    let s1 = b.snapshot().unwrap();
    let s2 = b.snapshot().unwrap();
    assert_eq!(s1.layers(), s2.layers());
    assert_eq!(s2.depth(), s1.depth() + 1);
}
