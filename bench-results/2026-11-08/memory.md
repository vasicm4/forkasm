- cpu: AMD Ryzen 7 7735HS with Radeon Graphics (16 threads)
- kernel: 7.0.0-38-generic
- wasmtime: 47.0.4
- forkasm: 0.1.0, release build
- rustc: rustc 1.99.1 (58cf8f9c3 2026-10-13)

### Memory per branch: 64 MiB snapshot, N live children (PSS from /proc/self/smaps_rollup)

| strategy | N | dirty pages/child | PSS/child KiB | VMAs/child |
|---|---|---|---|---|
| S0 | 1 | 0 | 65600 | 2 |
| S0 | 1 | 16 | 65600 | 2 |
| S0 | 10 | 0 | 65600 | 2 |
| S0 | 10 | 16 | 65600 | 2 |
| S1 | 1 | 0 | 0 | 8 |
| S1 | 1 | 16 | 68 | 8 |
| S1 | 10 | 0 | 0 | 8 |
| S1 | 10 | 16 | 68 | 8 |
| S1 | 100 | 0 | 0.2 | 8 |
| S1 | 100 | 16 | 68 | 8 |
| S1 | 1000 | 0 | 0 | 8 |
| S1 | 1000 | 16 | 68 | 8 |

- includes each child's Wasmtime instance (store, vmctx, tables) besides its memory
