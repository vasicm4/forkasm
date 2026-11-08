//! The branch driver and `run_parallel` (SPEC §5.6, §5.7).

mod common;

use common::{kv_wasm, py_root, runtime, strategies};
use forkasm::driver::{Branch, Search, search};
use forkasm::{Runtime, Sandbox, SpawnOpts, Strategy, Val};

fn kv_root(rt: &Runtime) -> Sandbox {
    let module = rt.load(kv_wasm()).unwrap();
    let mut root = rt.spawn(&module, SpawnOpts::default()).unwrap();
    root.call("init", &[]).unwrap();
    root
}

#[test]
fn run_parallel_keeps_order_and_isolation() {
    for strategy in strategies() {
        let rt = runtime(strategy);
        let mut root = kv_root(&rt);
        root.call("put", &[Val::I64(1), Val::I64(7)]).unwrap();
        let kids = rt.spawn_many(&root.snapshot().unwrap(), 16).unwrap();
        let results = rt.run_parallel(kids, |i, sb| {
            sb.call("put", &[Val::I64(1), Val::I64(100 + i as i64)]).unwrap();
            sb.call("touch_pages", &[Val::I32(0), Val::I32(4), Val::I32(i as i32)]).unwrap();
            i
        });
        for (i, (mut sb, r)) in results.into_iter().enumerate() {
            assert_eq!(r, i);
            assert_eq!(sb.call_u64("get", &[Val::I64(1)]).unwrap(), 100 + i as u64, "{strategy:?}");
        }
        assert_eq!(root.call_u64("get", &[Val::I64(1)]).unwrap(), 7);
    }
}

/// A step that mutates state (so children inherit it) and scores by the
/// branch's own random draw plus the inherited total.
fn kv_step(sb: &mut Sandbox, b: &Branch) -> anyhow::Result<f64> {
    let r = sb.call_u64("rand", &[])? % 1000;
    let total = sb.call_u64("get", &[Val::I64(0)])? + r;
    sb.call("put", &[Val::I64(0), Val::I64(total as i64)])?;
    sb.call("touch_pages", &[Val::I32(b.level as i32 * 8), Val::I32(4), Val::I32(r as i32)])?;
    if b.index == 3 {
        anyhow::bail!("branch 3 always fails");
    }
    Ok(total as f64)
}

/// The same search done one branch at a time with plain spawn/snapshot.
fn sequential(rt: &Runtime, root: &forkasm::Snapshot, shape: Search) -> Vec<(f64, Vec<usize>)> {
    let mut frontier = vec![(root.clone(), Vec::new())];
    let mut last = Vec::new();
    for level in 0..shape.depth {
        let mut ok = Vec::new();
        for (snap, path) in &frontier {
            for index in 0..shape.branching {
                let mut sb = rt.spawn_from(snap, index as u64).unwrap();
                let path: Vec<usize> = path.iter().copied().chain([index]).collect();
                if let Ok(score) = kv_step(&mut sb, &Branch { level, index, path: path.clone() }) {
                    ok.push((sb, score, path));
                }
            }
        }
        ok.sort_by(|a, b| b.1.total_cmp(&a.1));
        ok.truncate(shape.keep);
        last = ok.iter().map(|(_, s, p)| (*s, p.clone())).collect();
        frontier = ok.into_iter().map(|(mut sb, _, p)| (sb.snapshot().unwrap(), p)).collect();
    }
    last
}

#[test]
fn search_matches_sequential_and_is_deterministic() {
    let shape = Search { branching: 5, keep: 3, depth: 4 };
    for strategy in strategies() {
        let rt = runtime(strategy);
        let mut root = kv_root(&rt);
        root.call("buf_init", &[Val::I32(64)]).unwrap();
        // The running total `kv_step` adds to (a missing key reads as u64::MAX).
        root.call("put", &[Val::I64(0), Val::I64(0)]).unwrap();
        let snap = root.snapshot().unwrap();
        let a = search(&snap, shape, kv_step).unwrap();
        let b = search(&snap, shape, kv_step).unwrap();
        let summary = |o: &forkasm::driver::Outcome| -> Vec<(f64, Vec<usize>)> {
            o.best.iter().map(|s| (s.score, s.path.clone())).collect()
        };
        assert_eq!(summary(&a), summary(&b), "{strategy:?}: not deterministic");
        assert_eq!(summary(&a), sequential(&rt, &snap, shape), "{strategy:?}");
        assert_eq!(a.best.len(), 3);
        assert_eq!(a.branches(), 5 + 3 * 5 * 3);
        assert!(a.levels.iter().all(|l| l.failed == l.branches / 5), "one in five branches fails");
        for (x, y) in a.best.iter().zip(&b.best) {
            assert!(x.sandbox.memory_view() == y.sandbox.memory_view(), "same path, different memory");
        }
    }
}

#[test]
fn search_with_all_branches_failing_ends_early() {
    let rt = runtime(Strategy::Layered);
    let snap = kv_root(&rt).snapshot().unwrap();
    let out =
        search(&snap, Search { branching: 4, keep: 2, depth: 5 }, |_, _| anyhow::bail!("nope")).unwrap();
    assert!(out.best.is_empty());
    assert_eq!(out.levels.len(), 1);
    assert_eq!(out.levels[0].failed, 4);
}

/// CPython branches in parallel: each level appends a digit; the score is
/// closeness to the target's prefix of that length, so the beam converges on
/// the target.
#[test]
fn python_beam_search_finds_target() {
    let rt = common::py_runtime(Strategy::Layered);
    let mut root = py_root(&rt);
    forkasm::python::exec(&mut root, "x = 0").unwrap();
    let snap = root.snapshot().unwrap();
    let shape = Search { branching: 10, keep: 2, depth: 4 };
    let out = search(&snap, shape, |sb, b| {
        let prefix = 4207 / 10i64.pow(3 - b.level as u32);
        forkasm::python::exec(sb, &format!("x = x * 10 + {}\nscore = -abs(x - {prefix})", b.index))?;
        Ok(forkasm::python::get_int(sb, "score")?.unwrap() as f64)
    })
    .unwrap();
    let best = &out.best[0];
    assert_eq!(best.score, 0.0);
    assert_eq!(best.path, [4, 2, 0, 7]);
    let mut sb = out.best.into_iter().next().unwrap().sandbox;
    assert_eq!(forkasm::python::get_int(&mut sb, "x").unwrap(), Some(4207));
}
