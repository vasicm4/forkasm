//! The benchmark suite (SPEC §8). Each benchmark returns a [`Table`]; `run`
//! writes every table as Markdown and CSV, headed by the environment.
//!
//! `--quick` shrinks sizes and iteration counts so the whole suite runs in
//! seconds: a smoke test that every benchmark works, not numbers to publish.

use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use forkasm::driver::{Search, search};
use forkasm::mem::{DirtyTracking, PoolConfig};
use forkasm::{Compaction, Config, Runtime, Sandbox, Snapshot, SpawnOpts, Strategy, Val};

pub const MIB: usize = 1 << 20;
const PAGE: usize = 4096;

/// Guest modules the suite runs.
pub struct Guests<'a> {
    pub kv: &'a str,
    pub py: &'a str,
    pub py_lib: &'a str,
}

/// Scale of a run.
#[derive(Clone, Copy)]
pub struct Scale {
    pub quick: bool,
}

impl Scale {
    /// `full` iterations normally, a handful in quick mode.
    fn iters(self, full: usize) -> usize {
        if self.quick { full.clamp(1, 5) } else { full }
    }
}

/// Runs every benchmark and writes `<name>.md`/`<name>.csv` into `out`.
pub fn run(guests: &Guests, out: &Path, scale: Scale) -> Result<()> {
    std::fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
    let env = environment(scale);
    std::fs::write(out.join("environment.md"), &env)?;
    println!("{env}");
    type Bench<'a> = (&'a str, Box<dyn Fn() -> Result<Table> + 'a>);
    let benches: Vec<Bench> = vec![
        ("strategies", Box::new(|| strategies(guests.kv, scale.iters(1000)))),
        ("size", Box::new(|| size(guests.kv, scale))),
        ("dirty", Box::new(|| dirty(guests.kv, scale))),
        ("memory", Box::new(|| memory(guests.kv, scale))),
        ("depth", Box::new(|| depth(guests.kv, if scale.quick { 16 } else { 256 }))),
        ("lazy", Box::new(|| lazy(guests.kv, scale))),
        ("python", Box::new(|| python_tree(guests, scale))),
    ];
    for (name, bench) in benches {
        let t = Instant::now();
        let table = bench().with_context(|| format!("benchmark {name}"))?;
        table.print();
        println!("({name}: {:.1} s)\n", t.elapsed().as_secs_f64());
        std::fs::write(out.join(format!("{name}.md")), format!("{env}\n{}", table.markdown()))?;
        std::fs::write(out.join(format!("{name}.csv")), table.csv())?;
    }
    println!("results in {}", out.display());
    Ok(())
}

/// Machine, kernel and versions, recorded with every result (SPEC §8).
pub fn environment(scale: Scale) -> String {
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .map(|l| l[l.find(':').unwrap() + 1..].trim().to_string())
        })
        .unwrap_or_else(|| "unknown".into());
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let rustc = std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let mut s = String::new();
    let _ =
        writeln!(s, "- cpu: {cpu} ({} threads)", std::thread::available_parallelism().map_or(0, |n| n.get()));
    let _ = writeln!(s, "- kernel: {}", kernel.trim());
    let _ = writeln!(s, "- wasmtime: {}", wasmtime_version());
    let _ = writeln!(
        s,
        "- forkasm: {}, {}",
        env!("CARGO_PKG_VERSION"),
        if cfg!(debug_assertions) { "debug build" } else { "release build" }
    );
    let _ = writeln!(s, "- rustc: {rustc}");
    if scale.quick {
        let _ = writeln!(s, "- **quick run: tiny sizes and iteration counts; not for publishing**");
    }
    s
}

fn wasmtime_version() -> &'static str {
    let lock = include_str!("../../../Cargo.lock");
    lock.split("[[package]]")
        .find(|p| p.contains("\nname = \"wasmtime\"\n"))
        .and_then(|p| p.lines().find_map(|l| l.strip_prefix("version = \"")))
        .map_or("unknown", |v| v.trim_end_matches('"'))
}

// ---------------------------------------------------------------- tables

/// Timing samples.
#[derive(Default)]
pub struct Samples(Vec<Duration>);

impl Samples {
    pub fn push(&mut self, d: Duration) {
        self.0.push(d);
    }

    fn quantile(&mut self, q: f64) -> Duration {
        self.0.sort();
        self.0[((self.0.len() - 1) as f64 * q).round() as usize]
    }

    fn cell(mut self) -> Cell {
        if self.0.is_empty() {
            Cell::Text("–".into())
        } else {
            Cell::Time(self.quantile(0.5), self.quantile(0.99))
        }
    }
}

pub enum Cell {
    Text(String),
    Num(f64),
    /// Median and p99.
    Time(Duration, Duration),
}

impl From<&str> for Cell {
    fn from(s: &str) -> Cell {
        Cell::Text(s.into())
    }
}

impl From<String> for Cell {
    fn from(s: String) -> Cell {
        Cell::Text(s)
    }
}

impl From<usize> for Cell {
    fn from(n: usize) -> Cell {
        Cell::Num(n as f64)
    }
}

impl From<f64> for Cell {
    fn from(n: f64) -> Cell {
        Cell::Num(n)
    }
}

impl From<Samples> for Cell {
    fn from(s: Samples) -> Cell {
        s.cell()
    }
}

pub struct Table {
    title: String,
    columns: Vec<&'static str>,
    rows: Vec<Vec<Cell>>,
    notes: Vec<String>,
}

impl Table {
    fn new(title: impl Into<String>, columns: &[&'static str]) -> Table {
        Table { title: title.into(), columns: columns.to_vec(), rows: Vec::new(), notes: Vec::new() }
    }

    fn row(&mut self, cells: Vec<Cell>) {
        assert_eq!(cells.len(), self.columns.len());
        self.rows.push(cells);
    }

    fn note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }

    fn markdown(&self) -> String {
        let mut s = format!("### {}\n\n| {} |\n|", self.title, self.columns.join(" | "));
        s.push_str(&"---|".repeat(self.columns.len()));
        s.push('\n');
        for row in &self.rows {
            let cells: Vec<String> = row.iter().map(show).collect();
            let _ = writeln!(s, "| {} |", cells.join(" | "));
        }
        if !self.notes.is_empty() {
            s.push('\n');
            for n in &self.notes {
                let _ = writeln!(s, "- {n}");
            }
        }
        s
    }

    /// Time cells become `<column>_med_us,<column>_p99_us`.
    fn csv(&self) -> String {
        let timed = |i: usize| self.rows.iter().any(|r| matches!(r[i], Cell::Time(..)));
        let header: Vec<String> = (0..self.columns.len())
            .map(|i| {
                let c = self.columns[i].replace([' ', '/'], "_");
                if timed(i) { format!("{c}_med_us,{c}_p99_us") } else { c }
            })
            .collect();
        let mut s = header.join(",") + "\n";
        for row in &self.rows {
            let cells: Vec<String> = row
                .iter()
                .enumerate()
                .map(|(i, c)| match c {
                    Cell::Text(t) if timed(i) => format!("{t},{t}"),
                    Cell::Text(t) => t.replace(',', ";"),
                    Cell::Num(n) => format!("{n}"),
                    Cell::Time(m, p) => format!("{:.2},{:.2}", m.as_secs_f64() * 1e6, p.as_secs_f64() * 1e6),
                })
                .collect();
            s.push_str(&cells.join(","));
            s.push('\n');
        }
        s
    }

    pub fn print(&self) {
        let cells: Vec<Vec<String>> = std::iter::once(self.columns.iter().map(|c| c.to_string()).collect())
            .chain(self.rows.iter().map(|r| r.iter().map(show).collect()))
            .collect();
        let widths: Vec<usize> = (0..self.columns.len())
            .map(|i| cells.iter().map(|r| r[i].chars().count()).max().unwrap())
            .collect();
        println!("{}\n", self.title);
        for row in cells {
            let line: Vec<String> = row
                .iter()
                .zip(&widths)
                .enumerate()
                .map(|(i, (c, w))| if i == 0 { format!("{c:<w$}") } else { format!("{c:>w$}") })
                .collect();
            println!("{}", line.join("  "));
        }
        for n in &self.notes {
            println!("  note: {n}");
        }
    }
}

fn show(c: &Cell) -> String {
    match c {
        Cell::Text(t) => t.clone(),
        Cell::Num(n) if n.fract() == 0.0 => format!("{n}"),
        Cell::Num(n) => format!("{n:.1}"),
        Cell::Time(m, p) => format!("{} / {}", fmt(*m), fmt(*p)),
    }
}

pub fn fmt(d: Duration) -> String {
    let us = d.as_secs_f64() * 1e6;
    if us >= 1e6 {
        format!("{:.2} s", us / 1e6)
    } else if us >= 1000.0 {
        format!("{:.2} ms", us / 1000.0)
    } else {
        format!("{us:.1} µs")
    }
}

// ---------------------------------------------------------------- kv setup

/// The default configuration, with compiled modules cached in Wasmtime's
/// user cache directory (CPython takes seconds to compile).
pub fn base_config() -> Config {
    Config { compile_cache: forkasm::CompileCache::UserDefault, ..Config::default() }
}

fn kv_runtime(
    strategy: Strategy,
    dirty_tracking: DirtyTracking,
    layer_pool: Option<PoolConfig>,
) -> Result<Runtime> {
    Runtime::new(Config { strategy, dirty_tracking, layer_pool, ..base_config() })
}

/// A clean snapshot of a kv guest with `buf_pages` buffer pages, all touched,
/// so its memory is fully populated.
fn kv_base(rt: &Runtime, kv: &str, buf_pages: usize) -> Result<Snapshot> {
    let module = rt.load(kv)?;
    let mut root = rt.spawn(&module, SpawnOpts::default())?;
    root.call("init", &[])?;
    root.call("buf_init", &[Val::I32(buf_pages as i32)])?;
    root.call("touch_pages", &[Val::I32(0), Val::I32(buf_pages as i32), Val::I32(1)])?;
    let snap = root.snapshot()?;
    rt.warm_up()?;
    Ok(snap)
}

/// Buffer pages for a total memory of about `mib` MiB (the rest of the
/// memory is ~1 MiB of kv's own data and stack).
fn buf_pages(mib: usize) -> usize {
    (mib.saturating_sub(1).max(1) * MIB / PAGE).max(16)
}

/// Spawns a parent from `base` and dirties `n` contiguous pages, starting at a
/// page that varies with `i`.
fn dirtied(rt: &Runtime, base: &Snapshot, i: u64, buf_pages: usize, n: usize) -> Result<Sandbox> {
    let mut p = rt.spawn_from(base, i)?;
    let start = (i as usize * 997) % (buf_pages - n).max(1);
    p.call("touch_pages", &[Val::I32(start as i32), Val::I32(n as i32), Val::I32(3)])?;
    Ok(p)
}

/// Snapshot, spawn and fork (= both) latency after dirtying `n` pages.
fn fork_samples(
    rt: &Runtime,
    base: &Snapshot,
    buf_pages: usize,
    n: usize,
    iters: usize,
) -> Result<[Samples; 3]> {
    let [mut snap, mut spawn, mut fork] = <[Samples; 3]>::default();
    for i in 0..iters as u64 {
        let mut parent = dirtied(rt, base, i, buf_pages, n)?;
        let t0 = Instant::now();
        let s = parent.snapshot()?;
        let t1 = Instant::now();
        let child = rt.spawn_from(&s, 0)?;
        let t2 = Instant::now();
        snap.push(t1 - t0);
        spawn.push(t2 - t1);
        fork.push(t2 - t0);
        drop((child, s, parent));
    }
    Ok([snap, spawn, fork])
}

/// Baseline B: process `fork()` of a host holding a sandbox spawned from
/// `base`. Returns (child ready, child dirtied one page and replied).
fn process_fork_samples(rt: &Runtime, base: &Snapshot, iters: usize) -> Result<[Samples; 2]> {
    let mut sb = rt.spawn_from(base, 0)?;
    let [mut ready, mut result] = <[Samples; 2]>::default();
    for i in 0..iters {
        let r = sb.process_fork_call("touch_pages", &[Val::I32(i as i32 % 8), Val::I32(1), Val::I32(5)])?;
        anyhow::ensure!(r.value == 1, "forked child failed its call");
        ready.push(r.ready);
        result.push(r.result);
    }
    Ok([ready, result])
}

// ---------------------------------------------------------------- benches

/// Every strategy at 64 MiB with 128 dirty pages (< 1%): the headline table.
pub fn strategies(kv: &str, iters: usize) -> Result<Table> {
    const MEM: usize = 64;
    const DIRTY: usize = 128;
    let pages = buf_pages(MEM);
    let mut t = Table::new(
        format!("Fork latency by strategy: {MEM} MiB, {DIRTY} dirty pages (median / p99)"),
        &["strategy", "iterations", "snapshot", "spawn", "fork"],
    );
    let pool = Some(PoolConfig::default());
    let cases = [
        ("S0 (full copy)", Strategy::Copy, DirtyTracking::PagemapScan, None),
        ("S1 pagemap read", Strategy::Layered, DirtyTracking::Pagemap, None),
        ("S1 PAGEMAP_SCAN", Strategy::Layered, DirtyTracking::PagemapScan, None),
        ("S1 scan + pool", Strategy::Layered, DirtyTracking::PagemapScan, pool),
        ("S1 write-protect", Strategy::Layered, DirtyTracking::WriteProtect, None),
        ("S1 write-protect + pool (default)", Strategy::Layered, DirtyTracking::WriteProtect, pool),
        ("S2 lazy (userfaultfd) + pool", Strategy::Lazy, DirtyTracking::WriteProtect, pool),
    ];
    let mut s1_base = None;
    for (name, strategy, tracking, layer_pool) in cases {
        let rt = match kv_runtime(strategy, tracking, layer_pool) {
            Ok(rt) => rt,
            Err(e) => {
                t.row(vec![
                    name.into(),
                    0.into(),
                    format!("unavailable: {e}").into(),
                    "–".into(),
                    "–".into(),
                ]);
                continue;
            }
        };
        let base = kv_base(&rt, kv, pages)?;
        let n = if strategy == Strategy::Copy { iters.min(100) } else { iters };
        let [snap, spawn, fork] = fork_samples(&rt, &base, pages, DIRTY, n)?;
        t.row(vec![name.into(), n.into(), snap.into(), spawn.into(), fork.into()]);
        // Process fork runs on the default S1 runtime (S2 cannot be forked).
        if strategy == Strategy::Layered && tracking == DirtyTracking::WriteProtect && layer_pool.is_some() {
            s1_base = Some((rt, base));
        }
    }
    if let Some((rt, base)) = s1_base {
        let n = iters.min(200);
        let [ready, result] = process_fork_samples(&rt, &base, n)?;
        t.row(vec!["process fork() (child ready)".into(), n.into(), "–".into(), "–".into(), ready.into()]);
        t.row(vec![
            "process fork() + 1-page write, reply".into(),
            n.into(),
            "–".into(),
            "–".into(),
            result.into(),
        ]);
        t.note("process fork() copies the page tables of the whole host process (here holding one 64 MiB sandbox)");
    }
    Ok(t)
}

/// Fork latency vs memory size at a fixed dirty fraction (0.78%).
pub fn size(kv: &str, scale: Scale) -> Result<Table> {
    let sizes: &[usize] = if scale.quick { &[1, 4] } else { &[1, 4, 16, 64, 256, 1024] };
    let mut t = Table::new(
        "Fork latency vs memory size, 0.78% of pages dirty (median / p99)",
        &["memory MiB", "dirty pages", "S0 fork", "S1 snapshot", "S1 spawn", "S1 fork", "process fork()"],
    );
    for &mib in sizes {
        let pages = buf_pages(mib);
        let dirty = (pages * 78 / 10_000).max(1);
        let s0 = kv_runtime(Strategy::Copy, DirtyTracking::Auto, None)?;
        let base = kv_base(&s0, kv, pages)?;
        // S0 copies everything: fewer iterations for big memories.
        let n0 = scale.iters((6400 / mib).clamp(5, 100));
        let [_, _, s0_fork] = fork_samples(&s0, &base, pages, dirty, n0)?;
        drop((base, s0));
        let s1 = Runtime::new(base_config())?;
        let base = kv_base(&s1, kv, pages)?;
        let [snap, spawn, fork] = fork_samples(&s1, &base, pages, dirty, scale.iters(500))?;
        let [ready, _] = process_fork_samples(&s1, &base, scale.iters(100))?;
        t.row(vec![
            mib.into(),
            dirty.into(),
            s0_fork.into(),
            snap.into(),
            spawn.into(),
            fork.into(),
            ready.into(),
        ]);
    }
    t.note("S1 = default configuration (write-protect tracking + layer pool); process fork() = until the child runs");
    Ok(t)
}

/// Fork latency vs dirty fraction at a fixed size.
pub fn dirty(kv: &str, scale: Scale) -> Result<Table> {
    let mib = if scale.quick { 4 } else { 64 };
    let pages = buf_pages(mib);
    let mut t = Table::new(
        format!("Fork latency vs dirty pages, {mib} MiB (median / p99)"),
        &["dirty pages", "dirty %", "S0 fork", "S1 snapshot", "S1 spawn", "S1 fork", "S1 maps/child"],
    );
    let s0 = kv_runtime(Strategy::Copy, DirtyTracking::Auto, None)?;
    let s0_base = kv_base(&s0, kv, pages)?;
    let s1 = Runtime::new(base_config())?;
    let s1_base = kv_base(&s1, kv, pages)?;
    let mut counts = vec![1, pages / 1000, pages / 100, pages / 10, pages / 2, pages];
    counts.retain(|&n| n > 0);
    counts.dedup();
    for n in counts {
        let [_, _, s0_fork] = fork_samples(&s0, &s0_base, pages, n, scale.iters(20))?;
        let [snap, spawn, fork] = fork_samples(&s1, &s1_base, pages, n, scale.iters(200))?;
        let maps = {
            let mut p = dirtied(&s1, &s1_base, 0, pages, n)?;
            s1.spawn_from(&p.snapshot()?, 0)?.stats().runs
        };
        let pct = 100.0 * n as f64 / pages as f64;
        t.row(vec![
            n.into(),
            format!("{pct:.2}").into(),
            s0_fork.into(),
            snap.into(),
            spawn.into(),
            fork.into(),
            maps.into(),
        ]);
    }
    t.note("dirty pages are contiguous; S1 = default configuration");
    Ok(t)
}

/// Extra memory per live branch, measured as PSS (SPEC §8).
pub fn memory(kv: &str, scale: Scale) -> Result<Table> {
    let mib = if scale.quick { 4 } else { 64 };
    let pages = buf_pages(mib);
    let counts: &[usize] = if scale.quick { &[1, 10] } else { &[1, 10, 100, 1000] };
    let mut t = Table::new(
        format!("Memory per branch: {mib} MiB snapshot, N live children (PSS from /proc/self/smaps_rollup)"),
        &["strategy", "N", "dirty pages/child", "PSS/child KiB", "VMAs/child"],
    );
    for (name, strategy) in [("S0", Strategy::Copy), ("S1", Strategy::Layered)] {
        let rt = kv_runtime(strategy, DirtyTracking::Auto, Some(PoolConfig::default()))?;
        let base = kv_base(&rt, kv, pages)?;
        for &n in counts {
            if strategy == Strategy::Copy && n > 10 {
                continue; // n full copies
            }
            for dirty in [0, 16] {
                let (pss0, vmas0) = (pss_kib()?, vma_count()?);
                let kids: Vec<Sandbox> = (0..n as u64)
                    .map(|i| {
                        let mut sb = rt.spawn_from(&base, i)?;
                        if dirty > 0 {
                            sb.call("touch_pages", &[Val::I32(i as i32 % 64), Val::I32(dirty), Val::I32(7)])?;
                        }
                        Ok(sb)
                    })
                    .collect::<Result<_>>()?;
                let (pss1, vmas1) = (pss_kib()?, vma_count()?);
                drop(kids);
                let per = (pss1 as f64 - pss0 as f64) / n as f64;
                let vmas = (vmas1 as f64 - vmas0 as f64) / n as f64;
                t.row(vec![name.into(), n.into(), (dirty as usize).into(), per.into(), vmas.into()]);
            }
        }
    }
    t.note("includes each child's Wasmtime instance (store, vmctx, tables) besides its memory");
    Ok(t)
}

fn pss_kib() -> Result<usize> {
    let s = std::fs::read_to_string("/proc/self/smaps_rollup")?;
    let line = s.lines().find(|l| l.starts_with("Pss:")).context("no Pss in smaps_rollup")?;
    Ok(line.split_whitespace().nth(1).context("bad Pss line")?.parse()?)
}

fn vma_count() -> Result<usize> {
    Ok(std::fs::read_to_string("/proc/self/maps")?.lines().count())
}

/// Fork-of-fork chain on ~64 MiB with 64 scattered dirty pages per level,
/// under several compaction settings.
pub fn depth(kv: &str, steps: usize) -> Result<Table> {
    let pages = buf_pages(64);
    let mut t = Table::new(
        format!(
            "Fork-of-fork chain: {steps} levels, 64 scattered dirty pages per level, 64 MiB (median / p99)"
        ),
        &["compaction", "snapshot", "spawn", "merges", "maps/child at end"],
    );
    let settings: [(&str, Option<Compaction>); 6] = [
        ("off", None),
        ("layers<=8", Some(Compaction { max_layers: 8, max_runs: usize::MAX, gap_pages: 16 })),
        ("layers<=32", Some(Compaction { max_layers: 32, max_runs: usize::MAX, gap_pages: 16 })),
        ("layers<=128", Some(Compaction { max_layers: 128, max_runs: usize::MAX, gap_pages: 16 })),
        ("maps<=1024", Some(Compaction { max_layers: usize::MAX, max_runs: 1024, gap_pages: 16 })),
        ("default (layers<=32, maps<=512, gap 16)", Some(Compaction::default())),
    ];
    for (name, compaction) in settings {
        let rt = Runtime::new(Config { compaction, ..base_config() })?;
        let base = kv_base(&rt, kv, pages)?;
        let mut sb = rt.spawn_from(&base, 0)?;
        let [mut snap_t, mut spawn_t] = <[Samples; 2]>::default();
        let mut merges = 0;
        let mut x: u64 = 0x9e37_79b9;
        for level in 0..steps {
            for _ in 0..64 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let page = (x % pages as u64) as i32;
                sb.call("touch_pages", &[Val::I32(page), Val::I32(1), Val::I32(level as i32)])?;
            }
            let layers_before = sb.stats().layers;
            let t0 = Instant::now();
            let s = sb.snapshot()?;
            let t1 = Instant::now();
            let child = rt.spawn_from(&s, level as u64)?;
            let t2 = Instant::now();
            // A plain snapshot adds a layer; compaction merges some.
            if s.layers() <= layers_before {
                merges += 1;
            }
            snap_t.push(t1 - t0);
            spawn_t.push(t2 - t1);
            sb = child;
        }
        t.row(vec![name.into(), snap_t.into(), spawn_t.into(), merges.into(), sb.stats().runs.into()]);
    }
    Ok(t)
}

/// End to end: the branch driver expanding a tree of CPython branches.
pub fn python_tree(guests: &Guests, scale: Scale) -> Result<Table> {
    use forkasm::python;
    let shape = if scale.quick {
        Search { branching: 4, keep: 2, depth: 2 }
    } else {
        Search { branching: 8, keep: 4, depth: 6 }
    };
    // Each branch reseeds from its own WASI RNG, does a little work and
    // scores by it.
    let step = |sb: &mut Sandbox, _: &forkasm::driver::Branch| -> Result<f64> {
        python::exec(
            sb,
            "random.seed()\nv = sorted(random.random() for _ in range(2000))\n\
             state.append(v[-1])\nscore = int(sum(state) * 1e6)",
        )?;
        Ok(python::get_int(sb, "score")?.context("no score")? as f64)
    };
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let mut t = Table::new(
        format!(
            "End to end: beam search over CPython branches (branching {}, keep {}, depth {})",
            shape.branching, shape.keep, shape.depth
        ),
        &["strategy", "threads", "repeats", "branches", "wall", "per branch", "fork share"],
    );
    let runs =
        [("S1", Strategy::Layered, 1), ("S1", Strategy::Layered, threads), ("S2", Strategy::Lazy, threads)];
    for (name, strategy, n) in runs {
        let rt = Runtime::new(Config { strategy, ..base_config() })?;
        let module = rt.load(guests.py)?;
        let mut root = python::spawn(&rt, &module, guests.py_lib)?;
        python::exec(&mut root, "import random\nstate = []")?;
        let snap = root.snapshot()?;
        rt.warm_up()?;
        let pool = rayon::ThreadPoolBuilder::new().num_threads(n).build()?;
        let mut wall = Samples::default();
        let mut branches = 0;
        let mut snapshot_time = Duration::ZERO;
        let mut total = Duration::ZERO;
        let repeats = scale.iters(10);
        for _ in 0..repeats {
            let t0 = Instant::now();
            let out = pool.install(|| search(&snap, shape, step))?;
            let d = t0.elapsed();
            wall.push(d);
            total += d;
            branches = out.branches();
            snapshot_time += out.levels.iter().map(|l| l.snapshot).sum::<Duration>();
        }
        let per_branch = total / (repeats * branches) as u32;
        let share = format!("{:.1}% snapshots", 100.0 * snapshot_time.as_secs_f64() / total.as_secs_f64());
        t.row(vec![
            name.into(),
            n.into(),
            repeats.into(),
            branches.into(),
            wall.into(),
            fmt(per_branch).into(),
            share.into(),
        ]);
    }
    t.note("each branch: spawn from its parent's snapshot, `random.seed()`, sort 2,000 random floats, score");
    t.note("fork share = time snapshotting survivors; spawn time is inside each branch's run");
    t.note("S2 serves every page fault from one handler thread");
    Ok(t)
}

/// S2 vs S1: fork latency against time to a first result, when the child
/// then writes some of its pages (SPEC §5.3).
pub fn lazy(kv: &str, scale: Scale) -> Result<Table> {
    let mib = if scale.quick { 4 } else { 64 };
    let pages = buf_pages(mib);
    const DIRTY: usize = 128;
    let mut t = Table::new(
        format!(
            "S1 vs S2: {mib} MiB, parent dirties {DIRTY} pages, child then writes N pages (median / p99)"
        ),
        &["child writes", "S1 fork", "S1 fork + writes", "S2 fork", "S2 fork + writes"],
    );
    let rts =
        [Runtime::new(base_config())?, Runtime::new(Config { strategy: Strategy::Lazy, ..base_config() })?];
    let bases = rts.iter().map(|rt| kv_base(rt, kv, pages)).collect::<Result<Vec<_>>>()?;
    let mut counts = vec![0, 16, pages / 100, pages / 10, pages];
    counts.sort();
    counts.dedup();
    for n in counts {
        let mut row: Vec<Cell> = vec![n.into()];
        for (rt, base) in rts.iter().zip(&bases) {
            let [mut fork, mut first] = <[Samples; 2]>::default();
            for i in 0..scale.iters(if n == pages { 50 } else { 300 }) as u64 {
                let mut parent = dirtied(rt, base, i, pages, DIRTY.min(pages / 2))?;
                let t0 = Instant::now();
                let s = parent.snapshot()?;
                let mut child = rt.spawn_from(&s, 0)?;
                let t1 = Instant::now();
                if n > 0 {
                    child.call("touch_pages", &[Val::I32(0), Val::I32(n as i32), Val::I32(9)])?;
                }
                let t2 = Instant::now();
                fork.push(t1 - t0);
                first.push(t2 - t0);
                drop((child, s, parent));
            }
            row.push(fork.into());
            row.push(first.into());
        }
        t.row(row);
    }
    t.note("S2 fills each page on first touch (a userfaultfd round trip); S1 maps the layers at spawn");
    Ok(t)
}
