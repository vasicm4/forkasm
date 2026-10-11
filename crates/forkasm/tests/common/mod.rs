//! Shared helpers for integration tests and benches.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use forkasm::mem::DirtyTracking;
use forkasm::{Config, Runtime, Strategy};

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
    configs
}
