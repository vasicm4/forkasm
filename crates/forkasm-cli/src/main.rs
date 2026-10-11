//! forkasm demo and benchmark harness.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use forkasm::mem::DirtyScan;
use forkasm::{Config, Runtime, Sandbox, Snapshot, SpawnOpts, Strategy, Val};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["envcheck"] => envcheck(),
        ["rewrite", path] => inspect(path),
        ["bench-fork", path] => bench_fork(path, 1000),
        ["bench-fork", path, iters] => bench_fork(path, iters.parse().context("iterations")?),
        _ => bail!("usage: forkasm-cli envcheck | rewrite <module.wasm> | bench-fork <kv.wasm> [iterations]"),
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
    let cases = [
        ("S0", Strategy::Copy, DirtyScan::PagemapScan),
        ("S1-pagemap", Strategy::Layered, DirtyScan::Pagemap),
        ("S1-pagemap_scan", Strategy::Layered, DirtyScan::PagemapScan),
    ];
    for (name, strategy, dirty_scan) in cases {
        let rt = Runtime::new(Config { strategy, dirty_scan, ..Config::default() })?;
        let module = rt.load(path)?;
        let mut root = rt.spawn(&module, SpawnOpts::default())?;
        root.call("init", &[])?;
        root.call("buf_init", &[Val::I32(BUF_PAGES)])?;
        root.call("touch_pages", &[Val::I32(0), Val::I32(BUF_PAGES), Val::I32(1)])?;
        let base = root.snapshot()?;
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
        println!(
            "{name:<18} {:>5.1} MiB {:>22} {:>22} {:>22}{}",
            base.memory_size() as f64 / MIB as f64,
            stats(&mut snap_t),
            stats(&mut spawn_t),
            stats(&mut fork_t),
            if n < iters { format!("  ({n} iterations)") } else { String::new() },
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
