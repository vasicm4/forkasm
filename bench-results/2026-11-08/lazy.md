- cpu: AMD Ryzen 7 7735HS with Radeon Graphics (16 threads)
- kernel: 7.0.0-38-generic
- wasmtime: 47.0.4
- forkasm: 0.1.0, release build
- rustc: rustc 1.99.1 (58cf8f9c3 2026-10-13)

### S1 vs S2: 64 MiB, parent dirties 128 pages, child then writes N pages (median / p99)

| child writes | S1 fork | S1 fork + writes | S2 fork | S2 fork + writes |
|---|---|---|---|---|
| 0 | 97.1 µs / 141.8 µs | 97.1 µs / 141.8 µs | 72.8 µs / 149.9 µs | 72.9 µs / 149.9 µs |
| 16 | 96.2 µs / 157.4 µs | 159.5 µs / 239.7 µs | 70.6 µs / 130.5 µs | 275.6 µs / 427.2 µs |
| 161 | 93.4 µs / 129.5 µs | 550.9 µs / 746.5 µs | 71.5 µs / 164.3 µs | 2.01 ms / 2.66 ms |
| 1612 | 96.1 µs / 143.6 µs | 5.36 ms / 6.72 ms | 81.1 µs / 157.2 µs | 19.36 ms / 23.73 ms |
| 16128 | 113.8 µs / 213.1 µs | 55.38 ms / 58.95 ms | 92.7 µs / 178.9 µs | 189.74 ms / 208.65 ms |

- S2 fills each page on first touch (a userfaultfd round trip); S1 maps the layers at spawn
