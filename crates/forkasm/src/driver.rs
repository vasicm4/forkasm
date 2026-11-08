//! Branch driver (SPEC §5.7): a beam search over sandboxes. Each level
//! forks every surviving node into `branching` children, runs a step in each
//! on a `rayon` pool, scores them, and keeps the best `keep`.

use std::time::{Duration, Instant};

use anyhow::Result;
use rayon::prelude::*;

use crate::{Runtime, Sandbox, Snapshot};

/// Shape of a search.
#[derive(Clone, Copy, Debug)]
pub struct Search {
    /// Children forked from each surviving node per level.
    pub branching: usize,
    /// Nodes kept after each level (the beam width).
    pub keep: usize,
    /// Levels to expand.
    pub depth: usize,
}

/// Where a branch sits in the tree, passed to the step function.
#[derive(Clone, Debug)]
pub struct Branch {
    /// 0-based level being expanded.
    pub level: usize,
    /// Which child of its parent this is (`0..branching`); also the RNG
    /// branch it was spawned with, so the same path replays identically.
    pub index: usize,
    /// Child indices from the root to this branch, inclusive.
    pub path: Vec<usize>,
}

/// A node kept at the end of the search.
pub struct Survivor {
    /// The live sandbox, ready to continue or snapshot again.
    pub sandbox: Sandbox,
    /// The score its last step returned.
    pub score: f64,
    /// Child indices from the root to this node.
    pub path: Vec<usize>,
}

/// Per-level numbers.
#[derive(Clone, Debug, Default)]
pub struct LevelStats {
    /// Branches run.
    pub branches: usize,
    /// Branches whose step returned an error or a NaN score (discarded).
    pub failed: usize,
    /// Wall time to spawn and run every branch.
    pub run: Duration,
    /// Wall time to snapshot the survivors (zero on the last level).
    pub snapshot: Duration,
}

/// The result of a [`search`].
pub struct Outcome {
    /// The best `keep` nodes of the last level, best first.
    pub best: Vec<Survivor>,
    /// Numbers for each level, root first.
    pub levels: Vec<LevelStats>,
}

impl Outcome {
    /// Branches run over the whole search.
    pub fn branches(&self) -> usize {
        self.levels.iter().map(|l| l.branches).sum()
    }
}

/// Runs a beam search from `root`. `step` advances a freshly forked branch
/// and returns its score (higher is better); a branch whose step fails is
/// dropped. Ties keep tree order, so a search is deterministic.
///
/// Errors only if spawning or snapshotting fails.
pub fn search(
    root: &Snapshot,
    shape: Search,
    step: impl Fn(&mut Sandbox, &Branch) -> Result<f64> + Sync,
) -> Result<Outcome> {
    anyhow::ensure!(shape.branching > 0 && shape.keep > 0, "branching and keep must be positive");
    let mut frontier: Vec<(Snapshot, Vec<usize>)> = vec![(root.clone(), Vec::new())];
    let mut levels = Vec::new();
    for level in 0..shape.depth {
        let t = Instant::now();
        let tasks: Vec<(&Snapshot, Branch)> = frontier
            .iter()
            .flat_map(|(snap, path)| {
                (0..shape.branching).map(move |index| {
                    let path = path.iter().copied().chain([index]).collect();
                    (snap, Branch { level, index, path })
                })
            })
            .collect();
        let branches = tasks.len();
        let ran: Vec<Option<(Sandbox, f64, Vec<usize>)>> = tasks
            .into_par_iter()
            .map(|(snap, branch)| {
                let mut sb = snap.spawn(branch.index as u64)?;
                Ok(match step(&mut sb, &branch) {
                    Ok(score) if !score.is_nan() => Some((sb, score, branch.path)),
                    _ => None,
                })
            })
            .collect::<Result<_>>()?;
        let mut ok: Vec<_> = ran.into_iter().flatten().collect();
        let failed = branches - ok.len();
        // Stable: equal scores keep tree order.
        ok.sort_by(|a, b| b.1.total_cmp(&a.1));
        ok.truncate(shape.keep);
        let run = t.elapsed();
        let mut stats = LevelStats { branches, failed, run, snapshot: Duration::ZERO };
        if level + 1 == shape.depth || ok.is_empty() {
            levels.push(stats);
            let best =
                ok.into_iter().map(|(sandbox, score, path)| Survivor { sandbox, score, path }).collect();
            return Ok(Outcome { best, levels });
        }
        let t = Instant::now();
        frontier =
            ok.into_par_iter().map(|(mut sb, _, path)| Ok((sb.snapshot()?, path))).collect::<Result<_>>()?;
        stats.snapshot = t.elapsed();
        levels.push(stats);
    }
    // depth == 0: nothing to expand.
    Ok(Outcome { best: Vec::new(), levels })
}

impl Runtime {
    /// Runs `f` on every sandbox in parallel (SPEC §5.6) and returns each
    /// sandbox with its result, in input order.
    pub fn run_parallel<R: Send>(
        &self,
        sandboxes: Vec<Sandbox>,
        f: impl Fn(usize, &mut Sandbox) -> R + Sync,
    ) -> Vec<(Sandbox, R)> {
        sandboxes
            .into_par_iter()
            .enumerate()
            .map(|(i, mut sb)| {
                let r = f(i, &mut sb);
                (sb, r)
            })
            .collect()
    }
}
