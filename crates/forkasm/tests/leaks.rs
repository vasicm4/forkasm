//! Leak test (SPEC §7): after dropping every sandbox, snapshot and runtime,
//! open memfds, VMAs and RSS return to baseline.
//!
//! Its own test binary, since it measures process-wide counters that tests
//! running in parallel would disturb.

mod common;

use std::time::{Duration, Instant};

use common::{kv_wasm, layered_configs};
use forkasm::{Config, Runtime, SpawnOpts, Strategy, Val};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Usage {
    memfds: usize,
    vmas: usize,
    rss_shmem_kib: u64,
    rss_anon_kib: u64,
}

fn usage() -> Usage {
    let memfds = std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
        .filter(|p| p.to_string_lossy().starts_with("/memfd:forkasm"))
        .count();
    let vmas = std::fs::read_to_string("/proc/self/maps").unwrap().lines().count();
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let field = |name: &str| {
        let line = status.lines().find(|l| l.starts_with(name)).unwrap();
        line.split_whitespace().nth(1).unwrap().parse::<u64>().unwrap()
    };
    Usage { memfds, vmas, rss_shmem_kib: field("RssShmem:"), rss_anon_kib: field("RssAnon:") }
}

/// A busy tree: a 64 MiB root, a fork-of-fork chain, wide fan-outs,
/// compactions and sibling drops.
fn workload(config: &Config) {
    let rt = Runtime::new(config.clone()).unwrap();
    let module = rt.load(kv_wasm()).unwrap();
    let mut root = rt.spawn(&module, SpawnOpts::default()).unwrap();
    root.call("init", &[]).unwrap();
    let pages = 16 * 1024;
    root.call("buf_init", &[Val::I32(pages)]).unwrap();
    root.call("touch_pages", &[Val::I32(0), Val::I32(pages), Val::I32(1)]).unwrap();
    let mut snaps = vec![root.snapshot().unwrap()];
    let mut sb = rt.spawn_from(&snaps[0], 0).unwrap();
    let levels = if cfg!(debug_assertions) { 6 } else { 20 };
    for level in 0..levels {
        sb.call("touch_pages", &[Val::I32(level * 300), Val::I32(200), Val::I32(level)]).unwrap();
        let s = if level % 7 == 6 { sb.snapshot_compacted() } else { sb.snapshot() }.unwrap();
        let kids = rt.spawn_many(&s, 8).unwrap();
        for (i, mut k) in kids.into_iter().enumerate() {
            k.call("touch_pages", &[Val::I32(i as i32 * 50), Val::I32(40), Val::I32(3)]).unwrap();
            if i % 3 == 0 {
                snaps.push(k.snapshot().unwrap());
            }
        }
        sb = rt.spawn_from(&s, level as u64).unwrap();
        snaps.push(s);
    }
    // Some snapshots outlive their sandboxes, some the other way round.
    snaps.truncate(5);
    drop(root);
    drop(snaps);
    drop(sb);
}

/// Waits for the pool's background thread to release dropped slabs.
fn settle(baseline: Usage) -> Usage {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let now = usage();
        if now.memfds <= baseline.memfds || Instant::now() > deadline {
            return now;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn everything_is_released() {
    let mut configs = layered_configs();
    configs.push(Config { strategy: Strategy::Copy, ..Config::default() });
    // Warm up process-wide state (the userfaultfd tracker, the pagemap fd,
    // allocator arenas, cached thread stacks) before taking the baseline.
    for c in &configs {
        workload(c);
    }
    let baseline = settle(usage());
    for c in &configs {
        for _ in 0..2 {
            workload(c);
        }
        let after = settle(baseline);
        let what = format!("{:?}/{:?}, pool {}", c.strategy, c.dirty_tracking, c.layer_pool.is_some());
        assert_eq!(after.memfds, baseline.memfds, "{what}: memfds {baseline:?} -> {after:?}");
        assert!(after.vmas <= baseline.vmas + 8, "{what}: VMAs {baseline:?} -> {after:?}");
        assert!(
            after.rss_shmem_kib <= baseline.rss_shmem_kib + 1024,
            "{what}: shmem {baseline:?} -> {after:?}"
        );
        assert!(
            after.rss_anon_kib <= baseline.rss_anon_kib + 32 * 1024,
            "{what}: anon {baseline:?} -> {after:?}"
        );
        eprintln!("{what}: {after:?} (baseline {baseline:?})");
    }
}
