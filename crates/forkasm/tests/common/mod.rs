//! Shared helpers for integration tests and benches.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use forkasm::mem::DirtyTracking;
use forkasm::{Compaction, CompileCache, Config, Runtime, Sandbox, Strategy, Val};

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

/// `config` with compiled modules cached under the target directory, so the
/// 19 MB CPython module compiles once across test binaries and runs.
pub fn cached(config: Config) -> Config {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("compile-cache");
    Config { compile_cache: CompileCache::Dir(dir), ..config }
}

/// A runtime for CPython tests (compile cache on).
pub fn py_runtime(strategy: Strategy) -> Runtime {
    Runtime::new(cached(Config { strategy, ..Config::default() })).unwrap()
}

/// S0, S1, and S2 when the kernel supports it.
pub fn strategies() -> Vec<Strategy> {
    let mut s = vec![Strategy::Copy, Strategy::Layered];
    if forkasm::mem::write_protect_available().is_ok() {
        s.push(Strategy::Lazy);
    } else {
        eprintln!("userfaultfd write-protect unavailable; S2 not tested");
    }
    s
}

/// Every S1 variant (each dirty-tracking mode, without a pool, with tight
/// compaction) and S2. Write-protect (and so S2) is skipped when the kernel
/// lacks it.
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
    // S2: lazy filling, with default and tight compaction.
    if forkasm::mem::write_protect_available().is_ok() {
        for compaction in [Some(Compaction::default()), Some(tight)] {
            configs.push(Config { strategy: Strategy::Lazy, compaction, ..Config::default() });
        }
    }
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

/// Builds `guests/py` (CPython 3.12, G2) once per process with
/// `guests/py/build.sh`, which downloads its toolchain on first use, and
/// returns the module path and the stdlib directory to mount at
/// [`forkasm::python::PY_LIB`].
pub fn py_wasm() -> &'static (PathBuf, PathBuf) {
    static PATHS: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    PATHS.get_or_init(|| {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../guests/py");
        let wasm = dir.join("out/py.wasm");
        let stale = |src: &str| {
            let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
            modified(&dir.join(src)) > modified(&wasm)
        };
        if !wasm.exists() || stale("driver.c") || stale("build.sh") {
            let status = Command::new(dir.join("build.sh")).status().expect("running guests/py/build.sh");
            assert!(status.success(), "building guests/py failed");
        }
        (wasm, dir.join("out/lib"))
    })
}

/// Spawns a G2 root with the stdlib mounted and the interpreter initialised.
pub fn py_root(rt: &Runtime) -> Sandbox {
    let (wasm, lib) = py_wasm();
    let module = rt.load(wasm).unwrap();
    forkasm::python::spawn(rt, &module, lib).unwrap()
}
