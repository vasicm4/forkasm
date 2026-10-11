//! Fork latency, S0 vs S1, at 64 MiB with < 1% of pages dirty (SPEC §10, Week 1).
//!
//! Each iteration starts from a fresh parent spawned from a clean, fully
//! populated 64 MiB snapshot, dirties `DIRTY` pages, then times
//! `snapshot()` + one `spawn_from()`. Setup and teardown are untimed.

#[path = "../tests/common/mod.rs"]
mod common;

use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use forkasm::mem::DirtyScan;
use forkasm::{Config, Runtime, Sandbox, Snapshot, SpawnOpts, Strategy, Val};

const MIB: usize = 1 << 20;
/// Buffer pages so the whole memory is ~64 MiB.
const BUF_PAGES: i32 = (63 * MIB / 4096) as i32;
/// 0.78% of 16384 pages.
const DIRTY: i32 = 128;

/// A clean snapshot of a kv guest whose 64 MiB memory is fully populated.
fn base(strategy: Strategy, dirty_scan: DirtyScan) -> (Runtime, Snapshot) {
    let rt = Runtime::new(Config { strategy, dirty_scan, ..Config::default() }).unwrap();
    let module = rt.load(common::kv_wasm()).unwrap();
    let mut root = rt.spawn(&module, SpawnOpts::default()).unwrap();
    root.call("init", &[]).unwrap();
    root.call("buf_init", &[Val::I32(BUF_PAGES)]).unwrap();
    root.call("touch_pages", &[Val::I32(0), Val::I32(BUF_PAGES), Val::I32(1)]).unwrap();
    let snap = root.snapshot().unwrap();
    let mb = snap.memory_size() as f64 / MIB as f64;
    eprintln!("{strategy:?}/{dirty_scan:?}: memory {mb:.1} MiB");
    (rt, snap)
}

fn dirty_parent(rt: &Runtime, snap: &Snapshot, i: u64) -> Sandbox {
    let mut p = rt.spawn_from(snap, i).unwrap();
    let start = (i * 997 % (BUF_PAGES - DIRTY) as u64) as i32;
    p.call("touch_pages", &[Val::I32(start), Val::I32(DIRTY), Val::I32(3)]).unwrap();
    p
}

fn fork_latency(c: &mut Criterion) {
    let mut g = c.benchmark_group("fork_64MiB_128_dirty_pages");
    g.sample_size(100).measurement_time(Duration::from_secs(10));
    let cases = [
        ("S0", Strategy::Copy, DirtyScan::PagemapScan),
        ("S1-pagemap", Strategy::Layered, DirtyScan::Pagemap),
        ("S1-pagemap_scan", Strategy::Layered, DirtyScan::PagemapScan),
    ];
    for (name, strategy, scan) in cases {
        let (rt, snap) = base(strategy, scan);
        g.bench_function(BenchmarkId::new("snapshot+spawn", name), |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for i in 0..iters {
                    let mut parent = dirty_parent(&rt, &snap, i);
                    let t = Instant::now();
                    let s = parent.snapshot().unwrap();
                    let child = rt.spawn_from(&s, 0).unwrap();
                    total += t.elapsed();
                    drop((child, s, parent));
                }
                total
            })
        });
        g.bench_function(BenchmarkId::new("snapshot", name), |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for i in 0..iters {
                    let mut parent = dirty_parent(&rt, &snap, i);
                    let t = Instant::now();
                    let s = parent.snapshot().unwrap();
                    total += t.elapsed();
                    drop((s, parent));
                }
                total
            })
        });
        g.bench_function(BenchmarkId::new("spawn", name), |b| {
            let mut parent = dirty_parent(&rt, &snap, 1);
            let s = parent.snapshot().unwrap();
            b.iter(|| rt.spawn_from(&s, 0).unwrap())
        });
    }
    g.finish();
}

criterion_group!(benches, fork_latency);
criterion_main!(benches);
