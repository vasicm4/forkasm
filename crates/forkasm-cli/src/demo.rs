//! End-to-end demo: an agent-style search over CPython branches.
//!
//! A root interpreter loads a task (sort 20,000 records by two keys) and a
//! checker. It is forked into one branch per candidate approach; the
//! branches run in parallel, the checker scores them, the fastest correct
//! branch survives and continues, and the others are dropped.

use std::time::Instant;

use anyhow::Result;
use forkasm::{Config, Runtime, python};

use crate::suite::fmt;

const SETUP: &str = r#"
import random, heapq, functools
random.seed(1234)
data = [(random.randrange(100), random.random()) for _ in range(20000)]
expected = sorted(data)
def check(out):
    return out == expected
"#;

/// (name, code leaving its answer in `out`).
const APPROACHES: [(&str, &str); 8] = [
    ("builtin sorted", "out = sorted(data)"),
    ("list.sort on a copy", "out = list(data)\nout.sort()"),
    ("heapq", "h = list(data)\nheapq.heapify(h)\nout = [heapq.heappop(h) for _ in range(len(h))]"),
    (
        "merge sort",
        "def ms(a):\n    if len(a) <= 1: return a\n    m = len(a) // 2\n    l, r = ms(a[:m]), ms(a[m:])\n    \
         o, i, j = [], 0, 0\n    while i < len(l) and j < len(r):\n        \
         if l[i] <= r[j]: o.append(l[i]); i += 1\n        else: o.append(r[j]); j += 1\n    \
         return o + l[i:] + r[j:]\nout = ms(data)",
    ),
    (
        "bucket by key, then sort",
        "b = [[] for _ in range(100)]\nfor k, v in data: b[k].append((k, v))\n\
         out = [x for bucket in b for x in sorted(bucket)]",
    ),
    ("cmp_to_key", "out = sorted(data, key=functools.cmp_to_key(lambda a, b: (a > b) - (a < b)))"),
    ("sort by first key only (wrong)", "out = sorted(data, key=lambda t: t[0])"),
    ("typo (raises)", "out = sortd(data)"),
];

pub fn run(wasm: &str, lib: &str) -> Result<()> {
    let rt = Runtime::new(Config::default())?;
    let t = Instant::now();
    let module = rt.load(wasm)?;
    println!("loaded CPython guest in {}", fmt(t.elapsed()));
    let t = Instant::now();
    let mut root = python::spawn(&rt, &module, lib)?;
    python::exec(&mut root, SETUP)?;
    println!("root interpreter + task set up in {}", fmt(t.elapsed()));
    rt.warm_up()?;

    let t = Instant::now();
    let snap = root.snapshot()?;
    let snapshot_time = t.elapsed();
    let t = Instant::now();
    let branches = rt.spawn_many(&snap, APPROACHES.len())?;
    let spawn_time = t.elapsed();
    println!(
        "forked {} branches: snapshot {}, spawning all {} ({:.1} MiB of memory each)\n",
        APPROACHES.len(),
        fmt(snapshot_time),
        fmt(spawn_time),
        snap.memory_size() as f64 / (1 << 20) as f64
    );

    let t = Instant::now();
    let results = rt.run_parallel(branches, |i, sb| {
        let t = Instant::now();
        let ran = python::exec(sb, APPROACHES[i].1);
        let elapsed = t.elapsed();
        match ran.and_then(|()| python::exec(sb, "ok = int(check(out))")) {
            Err(e) => Err(e.to_string().lines().last().unwrap_or("error").to_string()),
            Ok(()) => match python::get_int(sb, "ok") {
                Ok(Some(1)) => Ok(elapsed),
                _ => Err("wrong answer".to_string()),
            },
        }
    });
    println!("ran all branches in parallel in {}\n", fmt(t.elapsed()));

    println!("{:<34} {:>12}  verdict", "approach", "time");
    for (i, (_, verdict)) in results.iter().enumerate() {
        match verdict {
            Ok(d) => println!("{:<34} {:>12}  correct", APPROACHES[i].0, fmt(*d)),
            Err(e) => println!("{:<34} {:>12}  {e}", APPROACHES[i].0, "–"),
        }
    }
    let (winner, (mut sb, best)) = results
        .into_iter()
        .enumerate()
        .filter_map(|(i, (sb, v))| v.ok().map(|d| (i, (sb, d))))
        .min_by_key(|(_, (_, d))| *d)
        .ok_or_else(|| anyhow::anyhow!("no approach succeeded"))?;
    // Every other branch has been dropped by now, freeing its pages.
    println!("\nwinner: {} ({}); the other branches are gone", APPROACHES[winner].0, fmt(best));

    python::exec(&mut sb, "print('winner continues: first three =', out[:3])")?;
    print!("{}", String::from_utf8_lossy(&sb.stdout()));
    let after = rt.spawn_from(&sb.snapshot()?, 0)?;
    println!(
        "winner snapshotted again: depth {}, {} layers, {} mappings per child",
        after.stats().depth,
        after.stats().layers,
        after.stats().runs
    );
    Ok(())
}
