//! Shared helpers for integration tests and benches.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use forkasm::mem::DirtyTracking;
use forkasm::{Compaction, Config, Runtime, Sandbox, Strategy, Val};

/// Builds `guests/kv` for `wasm32-wasip1` (once per process) and returns the
/// module path.
pub fn kv_wasm() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../guests/kv");
        let status = Command::new(env!("CARGO"))
            .args(["build", "--quiet", "--release", "--target", "wasm32-wasip1", "--manifest-path"])
            .arg(dir.join("Cargo.toml"))
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .status()
            .expect("running cargo");
        assert!(
            status.success(),
            "building guests/kv failed; is the target installed? `rustup target add wasm32-wasip1`"
        );
        dir.join("target/wasm32-wasip1/release/kv.wasm")
    })
}

pub fn runtime(strategy: Strategy) -> Runtime {
    Runtime::new(Config { strategy, ..Config::default() }).unwrap()
}

pub const STRATEGIES: [Strategy; 2] = [Strategy::Copy, Strategy::Layered];

/// Every S1 variant: each dirty-tracking mode, and running without a pool.
/// Write-protect is skipped when the kernel lacks it.
pub fn layered_configs() -> Vec<Config> {
    let mut modes = vec![DirtyTracking::PagemapScan, DirtyTracking::Pagemap];
    if forkasm::mem::write_protect_available().is_ok() {
        modes.push(DirtyTracking::WriteProtect);
    } else {
        eprintln!("write-protect tracking unavailable; not tested");
    }
    let mut configs: Vec<Config> = modes
        .into_iter()
        .map(|dirty_tracking| Config { strategy: Strategy::Layered, dirty_tracking, ..Config::default() })
        .collect();
    configs.push(Config { strategy: Strategy::Layered, layer_pool: None, ..Config::default() });
    // Aggressive compaction, with and without write-protect tracking.
    let tight = Compaction { max_layers: 4, max_runs: 24, gap_pages: 2 };
    let tightened: Vec<Config> = configs
        .iter()
        .filter(|c| c.layer_pool.is_some() && c.dirty_tracking != DirtyTracking::Pagemap)
        .map(|c| Config { compaction: Some(tight), ..c.clone() })
        .collect();
    configs.extend(tightened);
    configs
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

/// Asserts two sandboxes are indistinguishable: memory, globals, WASI state
/// and filesystem.
pub fn assert_same(s0: &mut Sandbox, s1: &mut Sandbox, what: &str) {
    assert_eq!(s0.stats().memory_bytes, s1.stats().memory_bytes, "{what}: memory size");
    assert!(s0.memory_view() == s1.memory_view(), "{what}: memory bytes differ");
    let g = |s: &mut Sandbox| s.globals().iter().map(bits).collect::<Vec<_>>();
    assert_eq!(g(s0), g(s1), "{what}: globals");
    assert_eq!(s0.stdout(), s1.stdout(), "{what}: stdout");
    assert_eq!(s0.wasi().rng, s1.wasi().rng, "{what}: rng");
    assert_eq!(s0.wasi().clock.now_ns, s1.wasi().clock.now_ns, "{what}: clock");
    assert!(s0.wasi().vfs.walk() == s1.wasi().vfs.walk(), "{what}: filesystems differ");
    assert_eq!(s0.stats().open_fds, s1.stats().open_fds, "{what}: open fds");
}

/// Applies a deterministic batch of guest operations derived from `seed` to a
/// kv sandbox whose page buffer has `buf_pages` pages.
pub fn work(sb: &mut Sandbox, buf_pages: i32, seed: u64) {
    let pages = (seed % 7 + 1) as i32;
    let start = (seed * 37 % (buf_pages as u64 - 8)) as i32;
    sb.call("put", &[Val::I64(seed as i64 % 50), Val::I64(seed as i64)]).unwrap();
    sb.call("touch_pages", &[Val::I32(start), Val::I32(pages), Val::I32(seed as i32)]).unwrap();
    let _ = sb.call_u64("rand", &[]).unwrap();
    let _ = sb.call_u64("now", &[]).unwrap();
    if seed.is_multiple_of(13) {
        // Grow memory now and then.
        sb.call("alloc", &[Val::I32(70_000)]).unwrap();
    }
}
