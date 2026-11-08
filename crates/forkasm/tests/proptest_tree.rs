//! Property test (SPEC §7): random sequences of {call, write, snapshot,
//! spawn, drop, compact} across a sandbox tree, run under S0 and an S1
//! variant side by side; every touched sandbox must stay identical.
//!
//! `FORKASM_PROPTEST_CASES=10000 cargo test --release -p forkasm --test proptest_tree`
//! is the Week 2 bar; plain `cargo test` runs a smaller number of cases.
//! (Not `PROPTEST_CASES`: proptest applies that to every shard.)

mod common;

use std::sync::OnceLock;

use common::{assert_same, kv_wasm, layered_configs, work};
use forkasm::{Config, GuestModule, Runtime, Sandbox, Snapshot, SpawnOpts, Strategy as MemStrategy, Val};
use proptest::prelude::*;
use proptest::sample::Index;

const BUF: i32 = 128;
/// Pages of the scratch area the host writes into directly.
const SCRATCH_PAGES: usize = 8;

#[derive(Clone, Debug)]
enum Op {
    /// Guest work: store, page writes, RNG, clock, occasional growth.
    Work(Index, u16),
    /// Host write into the guest's scratch area.
    Write(Index, usize, u8),
    /// Guest file operation through `std::fs`: append to, or remove, one of a
    /// few files.
    File(Index, u8, u16),
    Snapshot(Index),
    SnapshotCompacted(Index),
    Spawn(Index, u8),
    /// `n` times: work, snapshot, continue in a child (a fork-of-fork chain).
    Chain(Index, usize),
    DropSandbox(Index),
    DropSnapshot(Index),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (any::<Index>(), any::<u16>()).prop_map(|(s, seed)| Op::Work(s, seed)),
        2 => (any::<Index>(), 0..SCRATCH_PAGES * 4096, any::<u8>()).prop_map(|(s, o, v)| Op::Write(s, o, v)),
        2 => (any::<Index>(), any::<u8>(), any::<u16>()).prop_map(|(s, f, seed)| Op::File(s, f, seed)),
        2 => any::<Index>().prop_map(Op::Snapshot),
        1 => any::<Index>().prop_map(Op::SnapshotCompacted),
        3 => (any::<Index>(), any::<u8>()).prop_map(|(s, b)| Op::Spawn(s, b)),
        2 => (any::<Index>(), 1..=40usize).prop_map(|(s, n)| Op::Chain(s, n)),
        1 => any::<Index>().prop_map(Op::DropSandbox),
        1 => any::<Index>().prop_map(Op::DropSnapshot),
    ]
}

/// Appends `seed`-derived bytes to `path`, or (1 in 5) removes it.
fn file_op(sb: &mut Sandbox, path: &str, seed: u16) {
    let data: Vec<u8> = (0..seed % 300).map(|i| (i ^ seed) as u8).collect();
    let mut arg = |bytes: &[u8]| {
        let ptr = sb.call_u64("alloc", &[Val::I32(bytes.len().max(1) as i32)]).unwrap();
        sb.write_memory(ptr as usize, bytes).unwrap();
        [Val::I32(ptr as i32), Val::I32(bytes.len() as i32)]
    };
    let p = arg(path.as_bytes());
    if seed.is_multiple_of(5) {
        sb.call("fs_remove", &p).unwrap();
    } else {
        let d = arg(&data);
        let rc = sb.call_u64("append_file", &[p[0], p[1], d[0], d[1]]).unwrap();
        assert_eq!(rc, 0);
    }
}

/// A runtime and the loaded guest, shared by all cases.
struct Env {
    rt: Runtime,
    module: GuestModule,
}

impl Env {
    fn new(config: Config) -> Env {
        let rt = Runtime::new(config).unwrap();
        let module = rt.load(kv_wasm()).unwrap();
        Env { rt, module }
    }
}

/// `[0]` is S0; the rest are the S1 variants.
fn envs() -> &'static [Env] {
    static ENVS: OnceLock<Vec<Env>> = OnceLock::new();
    ENVS.get_or_init(|| {
        let s0 = Config { strategy: MemStrategy::Copy, ..Config::default() };
        std::iter::once(s0).chain(layered_configs()).map(Env::new).collect()
    })
}

/// The same tree under two runtimes: `[S0, S1 variant]`.
struct Tree<'a> {
    envs: [&'a Env; 2],
    sandboxes: Vec<[Sandbox; 2]>,
    snapshots: Vec<[Snapshot; 2]>,
    /// Guest address of the scratch area (identical in both: deterministic).
    scratch: usize,
    max_depth: usize,
}

impl<'a> Tree<'a> {
    fn new(envs: [&'a Env; 2]) -> Tree<'a> {
        let mut scratch = 0;
        let root = envs.map(|e| {
            let mut sb = e.rt.spawn(&e.module, SpawnOpts { seed: 7, ..SpawnOpts::default() }).unwrap();
            sb.call("init", &[]).unwrap();
            sb.call("buf_init", &[Val::I32(BUF)]).unwrap();
            scratch = sb.call_u64("alloc", &[Val::I32((SCRATCH_PAGES * 4096) as i32)]).unwrap() as usize;
            sb
        });
        Tree { envs, sandboxes: vec![root], snapshots: Vec::new(), scratch, max_depth: 0 }
    }

    fn check(&mut self, i: usize, what: &str) {
        let [a, b] = &mut self.sandboxes[i];
        assert_same(a, b, what);
    }

    fn spawn(&mut self, snap: usize, branch: u64) -> usize {
        let s = &self.snapshots[snap];
        let pair = [0, 1].map(|k| self.envs[k].rt.spawn_from(&s[k], branch).unwrap());
        self.sandboxes.push(pair);
        self.sandboxes.len() - 1
    }

    fn snapshot(&mut self, i: usize, compacted: bool) -> usize {
        let pair = self.sandboxes[i]
            .each_mut()
            .map(|sb| if compacted { sb.snapshot_compacted() } else { sb.snapshot() }.unwrap());
        assert_eq!(pair[0].depth(), pair[1].depth());
        self.max_depth = self.max_depth.max(pair[0].depth());
        self.snapshots.push(pair);
        self.snapshots.len() - 1
    }

    fn apply(&mut self, op: &Op) {
        let n = self.sandboxes.len();
        match *op {
            Op::Work(ref s, seed) => {
                let i = s.index(n);
                for sb in &mut self.sandboxes[i] {
                    work(sb, BUF, seed as u64);
                }
                self.check(i, "work");
            }
            Op::Write(ref s, off, v) => {
                let i = s.index(n);
                for sb in &mut self.sandboxes[i] {
                    sb.write_memory(self.scratch + off, &[v]).unwrap();
                }
                self.check(i, "write");
            }
            Op::File(ref s, file, seed) => {
                let i = s.index(n);
                let path = format!("/f{}", file % 4);
                for sb in &mut self.sandboxes[i] {
                    file_op(sb, &path, seed);
                }
                self.check(i, "file");
            }
            Op::Snapshot(ref s) | Op::SnapshotCompacted(ref s) => {
                let i = s.index(n);
                let snap = self.snapshot(i, matches!(op, Op::SnapshotCompacted(_)));
                self.check(i, "parent after snapshot");
                let child = self.spawn(snap, 0);
                self.check(child, "child of new snapshot");
                self.sandboxes.pop();
            }
            Op::Spawn(ref s, branch) => {
                if self.snapshots.is_empty() {
                    return;
                }
                let child = self.spawn(s.index(self.snapshots.len()), branch as u64);
                self.check(child, "spawn");
            }
            Op::Chain(ref s, steps) => {
                let mut i = s.index(n);
                for step in 0..steps {
                    for sb in &mut self.sandboxes[i] {
                        work(sb, BUF, step as u64 * 31 + 3);
                    }
                    let snap = self.snapshot(i, false);
                    i = self.spawn(snap, step as u64);
                    self.check(i, "chain");
                }
            }
            Op::DropSandbox(ref s) => {
                if n > 1 {
                    self.sandboxes.swap_remove(s.index(n));
                }
            }
            Op::DropSnapshot(ref s) => {
                if !self.snapshots.is_empty() {
                    self.snapshots.swap_remove(s.index(self.snapshots.len()));
                }
            }
        }
    }
}

fn run(variant: Index, ops: Vec<Op>) {
    let envs = envs();
    let s1 = &envs[1 + variant.index(envs.len() - 1)];
    let mut tree = Tree::new([&envs[0], s1]);
    for op in &ops {
        tree.apply(op);
    }
    // Everything still alive still matches, and spawns the same.
    for i in 0..tree.sandboxes.len() {
        tree.check(i, "final");
    }
    // A sample of the snapshots still spawn the same (every one would
    // dominate the run time: chains leave many behind).
    let step = tree.snapshots.len().div_ceil(6).max(1);
    for k in (0..tree.snapshots.len()).rev().step_by(step) {
        let child = tree.spawn(k, 99);
        tree.check(child, "final respawn");
    }
    // `FORKASM_DEPTH_STATS=1` prints how deep each case's tree got.
    if std::env::var_os("FORKASM_DEPTH_STATS").is_some() {
        eprintln!("depth {}", tree.max_depth);
    }
}

/// Cases per shard: the total (`FORKASM_PROPTEST_CASES`, default 256 in
/// release, 24 in debug) split across the shards below, which the test
/// harness runs in parallel.
fn config() -> ProptestConfig {
    const SHARDS: u32 = 8;
    let total = std::env::var("FORKASM_PROPTEST_CASES").ok().and_then(|c| c.parse().ok());
    let total: u32 = total.unwrap_or(if cfg!(debug_assertions) { 24 } else { 256 });
    ProptestConfig { cases: total.div_ceil(SHARDS), ..ProptestConfig::default() }
}

macro_rules! shards {
    ($($name:ident),*) => {
        proptest! {
            #![proptest_config(config())]
            $(
                #[test]
                fn $name(variant in any::<Index>(), ops in proptest::collection::vec(op(), 1..60)) {
                    run(variant, ops);
                }
            )*
        }
    };
}

shards!(
    tree_matches_s0_0,
    tree_matches_s0_1,
    tree_matches_s0_2,
    tree_matches_s0_3,
    tree_matches_s0_4,
    tree_matches_s0_5,
    tree_matches_s0_6,
    tree_matches_s0_7
);
