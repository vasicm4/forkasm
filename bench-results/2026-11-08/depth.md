- cpu: AMD Ryzen 7 7735HS with Radeon Graphics (16 threads)
- kernel: 7.0.0-38-generic
- wasmtime: 47.0.4
- forkasm: 0.1.0, release build
- rustc: rustc 1.99.1 (58cf8f9c3 2026-10-13)

### Fork-of-fork chain: 256 levels, 64 scattered dirty pages per level, 64 MiB (median / p99)

| compaction | snapshot | spawn | merges | maps/child at end |
|---|---|---|---|---|
| off | 2.38 ms / 4.55 ms | 36.78 ms / 53.32 ms | 0 | 14023 |
| layers<=8 | 420.1 µs / 71.71 ms | 4.12 ms / 6.26 ms | 42 | 1217 |
| layers<=32 | 593.4 µs / 45.19 ms | 7.45 ms / 12.61 ms | 9 | 1506 |
| layers<=128 | 1.45 ms / 2.79 ms | 22.18 ms / 37.05 ms | 2 | 1 |
| maps<=1024 | 242.9 µs / 70.25 ms | 1.68 ms / 3.57 ms | 28 | 510 |
| default (layers<=32, maps<=512, gap 16) | 186.3 µs / 70.57 ms | 860.5 µs / 1.79 ms | 51 | 131 |
