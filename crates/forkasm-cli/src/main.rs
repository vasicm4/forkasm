//! forkasm demo and benchmark harness.

mod demo;
mod suite;

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use forkasm::{Compaction, Config, Runtime};
use suite::{Guests, MIB, Scale, fmt};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["envcheck"] => envcheck(),
        ["rewrite", path] => inspect(path),
        ["bench-fork", path] => print(suite::strategies(path, 1000)),
        ["bench-fork", path, iters] => print(suite::strategies(path, iters.parse().context("iterations")?)),
        ["bench-depth", path] => print(suite::depth(path, 256)),
        ["bench-depth", path, steps] => print(suite::depth(path, steps.parse().context("steps")?)),
        ["demo", py, py_lib] => demo::run(py, py_lib),
        ["bench-suite", kv, py, py_lib, out] => bench_suite(Guests { kv, py, py_lib }, out, false),
        ["bench-suite", kv, py, py_lib, out, "--quick"] => bench_suite(Guests { kv, py, py_lib }, out, true),
        ["py-profile", wasm, lib] => py_profile(wasm, lib, 200),
        ["py-profile", wasm, lib, iters] => py_profile(wasm, lib, iters.parse().context("iterations")?),
        _ => bail!(
            "usage: forkasm-cli envcheck | rewrite <module.wasm> | bench-fork <kv.wasm> [iterations] \
             | bench-depth <kv.wasm> [steps] | py-profile <py.wasm> <stdlib dir> [iterations] \
             | bench-suite <kv.wasm> <py.wasm> <stdlib dir> <out dir> [--quick] \
             | demo <py.wasm> <stdlib dir>"
        ),
    }
}

fn print(table: Result<suite::Table>) -> Result<()> {
    table?.print();
    Ok(())
}

fn bench_suite(guests: Guests, out: &str, quick: bool) -> Result<()> {
    suite::run(&guests, Path::new(out), Scale { quick })
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

/// G2 (CPython): how much memory each statement dirties, and fork latency
/// between statements, under the default S1 configuration.
fn py_profile(wasm: &str, lib: &str, iters: usize) -> Result<()> {
    use forkasm::python;
    let rt = Runtime::new(suite::base_config())?;
    let t = Instant::now();
    let module = rt.load(wasm)?;
    println!("load + compile: {}", fmt(t.elapsed()));
    let t = Instant::now();
    let mut sb = python::spawn(&rt, &module, lib)?;
    println!("root spawn + py_init: {}", fmt(t.elapsed()));
    let program = [
        "import json, re, collections, random",
        "x = 1",
        "x += 1",
        "words = [w for w in 'the quick brown fox jumps over the lazy dog'.split()] * 100",
        "c = collections.Counter(words)",
        "d = {i: str(i) for i in range(10000)}",
        "s = json.dumps(d)",
        "del d, s",
        "def f(n): return n if n < 2 else f(n - 1) + f(n - 2)\nr = f(18)",
        "print(c.most_common(1), r)",
        "big = bytearray(8 << 20)",
        "big[::4096] = b'x' * (len(big) // 4096)",
        "del big",
    ];
    let mut base = sb.snapshot()?;
    rt.warm_up()?;
    println!("memory after init: {:.1} MiB\n", base.memory_size() as f64 / MIB as f64);
    println!(
        "{:<44} {:>9} {:>11} {:>10} {:>10} {:>7} {:>6}",
        "statement", "exec", "dirty KiB", "snapshot", "spawn", "layers", "runs"
    );
    for stmt in program {
        let t0 = Instant::now();
        python::exec(&mut sb, stmt)?;
        let t1 = Instant::now();
        let snap = sb.snapshot()?;
        let t2 = Instant::now();
        let child = rt.spawn_from(&snap, 0)?;
        let t3 = Instant::now();
        let dirty = snap.bytes().saturating_sub(if snap.layers() > base.layers() { base.bytes() } else { 0 });
        let label: String = stmt.replace('\n', "; ").chars().take(44).collect();
        println!(
            "{label:<44} {:>9} {:>11} {:>10} {:>10} {:>7} {:>6}",
            fmt(t1 - t0),
            dirty / 1024,
            fmt(t2 - t1),
            fmt(t3 - t2),
            snap.layers(),
            child.stats().runs,
        );
        base = snap;
    }
    // Spawn cost without layer mappings: from a fully compacted snapshot.
    let flat = sb.snapshot_compacted()?;
    let mut flat_t: Vec<Duration> = (0..iters)
        .map(|i| {
            let t = Instant::now();
            let child = rt.spawn_from(&flat, i as u64);
            let d = t.elapsed();
            child.map(|_| d)
        })
        .collect::<Result<_>>()?;
    println!(
        "\nspawn from a compacted snapshot ({} maps): {}",
        rt.spawn_from(&flat, 0)?.stats().runs,
        stats(&mut flat_t)
    );
    drop(sb);

    // A fork-of-fork chain: each level runs a small statement, snapshots and
    // continues in a fresh child, under several compaction thresholds.
    println!("\nfork-of-fork chain, {iters} levels of small statements (median / p99):\n");
    println!(
        "{:<12} {:>22} {:>22} {:>22} {:>22} {:>8} {:>11}",
        "compaction", "snapshot", "spawn", "fork", "next spawn", "merges", "maps/child"
    );
    let settings: [(&str, Option<Compaction>); 6] = [
        ("off", None),
        ("default", Some(Compaction::default())),
        ("maps<=256", Some(Compaction { max_runs: 256, ..Compaction::default() })),
        ("maps<=128", Some(Compaction { max_runs: 128, ..Compaction::default() })),
        ("maps<=64", Some(Compaction { max_runs: 64, ..Compaction::default() })),
        ("maps<=32", Some(Compaction { max_runs: 32, ..Compaction::default() })),
    ];
    for (name, compaction) in settings {
        let rt = Runtime::new(Config { compaction, ..suite::base_config() })?;
        let module = rt.load(wasm)?;
        let mut sb = python::spawn(&rt, &module, lib)?;
        python::exec(&mut sb, "import json, collections\nx = 0\nlog = []\nd = {}")?;
        rt.warm_up()?;
        let (mut snap_t, mut spawn_t, mut fork_t) = (Vec::new(), Vec::new(), Vec::new());
        let mut next_t = Vec::new();
        let mut merges = 0;
        for level in 0..iters {
            let stmt = match level % 4 {
                0 => "x = x + 1",
                1 => "log.append(x)",
                2 => "d[x] = json.dumps(log[-10:])",
                _ => "c = collections.Counter(str(x))",
            };
            python::exec(&mut sb, stmt)?;
            let layers_before = sb.stats().layers;
            let t0 = Instant::now();
            let s = sb.snapshot()?;
            let t1 = Instant::now();
            let child = rt.spawn_from(&s, level as u64)?;
            let t2 = Instant::now();
            // Siblings reuse the snapshot's mapping plan.
            let sibling = rt.spawn_from(&s, level as u64 + 1)?;
            next_t.push(t2.elapsed());
            drop(sibling);
            if s.layers() <= layers_before {
                merges += 1;
            }
            snap_t.push(t1 - t0);
            spawn_t.push(t2 - t1);
            fork_t.push(t2 - t0);
            sb = child;
        }
        println!(
            "{name:<12} {:>22} {:>22} {:>22} {:>22} {merges:>8} {:>11}",
            stats(&mut snap_t),
            stats(&mut spawn_t),
            stats(&mut fork_t),
            stats(&mut next_t),
            sb.stats().runs
        );
    }
    Ok(())
}

fn stats(samples: &mut [Duration]) -> String {
    samples.sort();
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q).round() as usize];
    format!("{} / {}", fmt(at(0.5)), fmt(at(0.99)))
}
