//! VMA-limit test (SPEC §7): a workload whose children would together need
//! more mappings than `vm.max_map_count` allows runs fine with compaction.
//!
//! Every layer segment costs each spawned sandbox ~2 VMAs (it splits the base
//! mapping). Running out of VMAs makes *any* `mmap` in the process fail
//! (allocator, Wasmtime), which aborts rather than erroring, so the
//! uncompacted case is measured and extrapolated, not driven into the wall.
//! Its own test binary, since it uses a large share of the process's VMAs.

mod common;

use common::kv_wasm;
use forkasm::{Compaction, Config, Runtime, Sandbox, SpawnOpts, Val};

fn max_map_count() -> usize {
    std::fs::read_to_string("/proc/sys/vm/max_map_count").unwrap().trim().parse().unwrap()
}

fn vmas() -> usize {
    std::fs::read_to_string("/proc/self/maps").unwrap().lines().count()
}

/// A snapshot whose dirty pages alternate (one run per page): the worst case
/// for mappings.
fn fragmented(config: Config, pages: i32) -> (Runtime, forkasm::Snapshot) {
    let rt = Runtime::new(config).unwrap();
    let module = rt.load(kv_wasm()).unwrap();
    let mut root = rt.spawn(&module, SpawnOpts::default()).unwrap();
    root.call("init", &[]).unwrap();
    root.call("buf_init", &[Val::I32(pages)]).unwrap();
    let base = root.snapshot().unwrap();
    let mut sb = rt.spawn_from(&base, 0).unwrap();
    for p in (0..pages).step_by(2) {
        sb.call("touch_pages", &[Val::I32(p), Val::I32(1), Val::I32(1)]).unwrap();
    }
    let snap = sb.snapshot().unwrap();
    (rt, snap)
}

/// Every child computes the same checksum over the fragmented buffer.
fn check(kids: &mut [Sandbox]) {
    let sums: Vec<u64> = kids.iter_mut().map(|k| k.call_u64("checksum", &[]).unwrap()).collect();
    assert!(sums.iter().all(|&s| s == sums[0]));
}

#[test]
fn compaction_keeps_many_branches_under_the_vma_cap() {
    let cap = max_map_count();
    let pages = 8192; // 32 MiB buffer, 4096 alternating dirty pages.

    // Without compaction: measure what one child costs.
    let off = Config { compaction: None, ..Config::default() };
    let (rt, snap) = fragmented(off, pages);
    let before = vmas();
    let probe: Vec<Sandbox> = rt.spawn_many(&snap, 4).unwrap();
    let per_child = (vmas() - before) / probe.len();
    let reference = probe[0].memory();
    drop(probe);
    let wanted = cap * 3 / 2 / per_child + 1;
    eprintln!(
        "cap {cap}: {per_child} VMAs per uncompacted child, so {wanted} children would need {}",
        wanted * per_child
    );
    assert!(per_child > 4096, "fragmentation did not take: {per_child} VMAs per child");
    assert!(wanted * per_child > cap);
    drop((snap, rt));

    // With compaction the same workload's children are cheap.
    let on = Config {
        compaction: Some(Compaction { max_layers: 64, max_runs: 1024, gap_pages: 16 }),
        ..Config::default()
    };
    let (rt, snap) = fragmented(on, pages);
    let before = vmas();
    let mut kids = rt.spawn_many(&snap, wanted).unwrap();
    let used = vmas() - before;
    eprintln!("{wanted} compacted children: {used} VMAs ({} each)", used / wanted);
    assert!(vmas() < cap / 2, "{} VMAs in use, cap {cap}", vmas());
    // Compaction changed the mapping, not the contents.
    assert!(kids.iter().all(|k| k.memory_view() == &reference[..]));
    check(&mut kids);
}
