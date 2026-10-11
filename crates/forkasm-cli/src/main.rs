//! forkasm demo and benchmark harness.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use forkasm::mem::DirtyTracking;
use forkasm::{Compaction, Config, Runtime, Sandbox, Snapshot, SpawnOpts, Strategy, Val};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["envcheck"] => envcheck(),
        ["rewrite", path] => inspect(path),
        ["bench-fork", path] => bench_fork(path, 1000),
        ["bench-fork", path, iters] => bench_fork(path, iters.parse().context("iterations")?),
        ["bench-depth", path] => bench_depth(path, 256),
        ["bench-depth", path, steps] => bench_depth(path, steps.parse().context("steps")?),
        _ => bail!(
            "usage: forkasm-cli envcheck | rewrite <module.wasm> | bench-fork <kv.wasm> [iterations] \
             | bench-depth <kv.wasm> [steps]"
        ),
    }
}

fn envcheck() -> Result<()> {
    for p in forkasm::mem::probe::run_all() {
        println!("{p}");
    }
    Ok(())
}

/// Prints what the rewrite pass does to a module.
fn inspect(path: &str) -> Result<()> {
    let r = forkasm::rewrite::rewrite(&std::fs::read(path)?)?;
    let data: usize = r.image.segments.iter().map(|(_, b)| b.len()).sum();
    println!("memory:   {} bytes initial", r.image.min_bytes);
    println!("data:     {} segments, {data} bytes", r.image.segments.len());
    println!("globals:  {:?}", r.globals);
    println!("start:    {}", r.has_start);
    Ok(())
}

const MIB: usize = 1 << 20;

/// Fork latency at ~64 MiB with 128 dirty pages (< 1%), as in
/// `crates/forkasm/benches/fork.rs`, reporting median and p99.
fn bench_fork(path: &str, iters: usize) -> Result<()> {
    const BUF_PAGES: i32 = (63 * MIB / 4096) as i32;
    const DIRTY: i32 = 128;
    println!("fork latency: kv guest, {iters} iterations, {DIRTY} dirty pages\n");
    println!(
        "{:<18} {:>9} {:>22} {:>22} {:>22}",
        "strategy", "memory", "snapshot med / p99", "spawn med / p99", "fork med / p99"
    );
    let no_pool = None;
    let pool = Some(forkasm::mem::PoolConfig::default());
    let cases = [
        ("S0", Strategy::Copy, DirtyTracking::PagemapScan, no_pool),
        ("S1 pagemap", Strategy::Layered, DirtyTracking::Pagemap, no_pool),
        ("S1 scan", Strategy::Layered, DirtyTracking::PagemapScan, no_pool),
        ("S1 scan+pool", Strategy::Layered, DirtyTracking::PagemapScan, pool),
        ("S1 wp", Strategy::Layered, DirtyTracking::WriteProtect, no_pool),
        ("S1 wp+pool", Strategy::Layered, DirtyTracking::WriteProtect, pool),
    ];
    for (name, strategy, dirty_tracking, layer_pool) in cases {
        let rt = match Runtime::new(Config { strategy, dirty_tracking, layer_pool, ..Config::default() }) {
            Ok(rt) => rt,
            Err(e) => {
                println!("{name:<18} skipped: {e}");
                continue;
            }
        };
        let module = rt.load(path)?;
        let mut root = rt.spawn(&module, SpawnOpts::default())?;
        root.call("init", &[])?;
        root.call("buf_init", &[Val::I32(BUF_PAGES)])?;
        root.call("touch_pages", &[Val::I32(0), Val::I32(BUF_PAGES), Val::I32(1)])?;
        let base = root.snapshot()?;
        rt.warm_up()?;
        let n = if strategy == Strategy::Copy { iters.min(100) } else { iters };
        let (mut snap_t, mut spawn_t, mut fork_t) = (Vec::new(), Vec::new(), Vec::new());
        for i in 0..n as u64 {
            let mut parent = dirty(&rt, &base, i, BUF_PAGES, DIRTY)?;
            let t0 = Instant::now();
            let s = parent.snapshot()?;
            let t1 = Instant::now();
            let child = rt.spawn_from(&s, 0)?;
            let t2 = Instant::now();
            snap_t.push(t1 - t0);
            spawn_t.push(t2 - t1);
            fork_t.push(t2 - t0);
            drop((child, s, parent));
        }
        let mut notes = Vec::new();
        if n < iters {
            notes.push(format!("{n} iterations"));
        }
        if rt.layer_pool_misses() > 0 && layer_pool.is_some() {
            notes.push(format!("{} pool misses", rt.layer_pool_misses()));
        }
        println!(
            "{name:<18} {:>5.1} MiB {:>22} {:>22} {:>22}{}",
            base.memory_size() as f64 / MIB as f64,
            stats(&mut snap_t),
            stats(&mut spawn_t),
            stats(&mut fork_t),
            if notes.is_empty() { String::new() } else { format!("  ({})", notes.join(", ")) },
        );
    }
    Ok(())
}

/// Compaction trade-off: a fork-of-fork chain of `steps` levels on ~64 MiB,
/// each level dirtying 64 scattered pages, under several thresholds.
fn bench_depth(path: &str, steps: usize) -> Result<()> {
    const BUF_PAGES: i32 = (63 * MIB / 4096) as i32;
    println!("fork-of-fork chain: kv guest, {steps} levels, 64 scattered dirty pages per level\n");
    println!(
        "{:<24} {:>22} {:>22} {:>14} {:>12}",
        "compaction", "snapshot med / p99", "spawn med / p99", "compactions", "maps/child"
    );
    let settings: [(&str, Option<Compaction>); 6] = [
        ("off", None),
        ("layers<=8", Some(Compaction { max_layers: 8, max_runs: usize::MAX, gap_pages: 16 })),
        ("layers<=32", Some(Compaction { max_layers: 32, max_runs: usize::MAX, gap_pages: 16 })),
        ("layers<=128", Some(Compaction { max_layers: 128, max_runs: usize::MAX, gap_pages: 16 })),
        ("maps<=1024", Some(Compaction { max_layers: usize::MAX, max_runs: 1024, gap_pages: 16 })),
        ("default", Some(Compaction::default())),
    ];
    for (name, compaction) in settings {
        let rt = Runtime::new(Config { compaction, ..Config::default() })?;
        let module = rt.load(path)?;
        let mut sb = rt.spawn(&module, SpawnOpts::default())?;
        sb.call("init", &[])?;
        sb.call("buf_init", &[Val::I32(BUF_PAGES)])?;
        sb.call("touch_pages", &[Val::I32(0), Val::I32(BUF_PAGES), Val::I32(1)])?;
        let base = sb.snapshot()?;
        sb = rt.spawn_from(&base, 0)?;
        rt.warm_up()?;
        let (mut snap_t, mut spawn_t) = (Vec::new(), Vec::new());
        let mut compactions = 0;
        let mut x: u64 = 0x9e37_79b9;
        for level in 0..steps {
            for _ in 0..64 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let page = (x % BUF_PAGES as u64) as i32;
                sb.call("touch_pages", &[Val::I32(page), Val::I32(1), Val::I32(level as i32)])?;
            }
            let layers_before = sb.stats().layers;
            let t0 = Instant::now();
            let s = sb.snapshot()?;
            let t1 = Instant::now();
            let child = rt.spawn_from(&s, level as u64)?;
            let t2 = Instant::now();
            if s.layers() <= 1 && layers_before >= 1 {
                compactions += 1;
            }
            snap_t.push(t1 - t0);
            spawn_t.push(t2 - t1);
            sb = child;
        }
        println!(
            "{name:<24} {:>22} {:>22} {compactions:>14} {:>12}",
            stats(&mut snap_t),
            stats(&mut spawn_t),
            sb.stats().runs
        );
    }
    Ok(())
}

fn dirty(rt: &Runtime, base: &Snapshot, i: u64, buf_pages: i32, n: i32) -> Result<Sandbox> {
    let mut p = rt.spawn_from(base, i)?;
    let start = (i * 997 % (buf_pages - n) as u64) as i32;
    p.call("touch_pages", &[Val::I32(start), Val::I32(n), Val::I32(3)])?;
    Ok(p)
}

fn stats(samples: &mut [Duration]) -> String {
    samples.sort();
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q).round() as usize];
    format!("{} / {}", fmt(at(0.5)), fmt(at(0.99)))
}

fn fmt(d: Duration) -> String {
    let us = d.as_secs_f64() * 1e6;
    if us >= 1000.0 { format!("{:.2} ms", us / 1000.0) } else { format!("{us:.1} µs") }
}
