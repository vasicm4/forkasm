- cpu: AMD Ryzen 7 7735HS with Radeon Graphics (16 threads)
- kernel: 7.0.0-38-generic
- wasmtime: 47.0.4
- forkasm: 0.1.0, release build
- rustc: rustc 1.99.1 (58cf8f9c3 2026-10-13)

### End to end: beam search over CPython branches (branching 8, keep 4, depth 6)

| strategy | threads | repeats | branches | wall | per branch | fork share |
|---|---|---|---|---|---|---|
| S1 | 1 | 10 | 168 | 367.94 ms / 373.99 ms | 2.19 ms | 1.0% snapshots |
| S1 | 16 | 10 | 168 | 132.50 ms / 136.41 ms | 788.4 µs | 3.9% snapshots |
| S2 | 16 | 10 | 168 | 142.00 ms / 144.98 ms | 847.9 µs | 1.8% snapshots |

- each branch: spawn from its parent's snapshot, `random.seed()`, sort 2,000 random floats, score
- fork share = time snapshotting survivors; spawn time is inside each branch's run
- S2 serves every page fault from one handler thread
