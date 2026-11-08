- cpu: AMD Ryzen 7 7735HS with Radeon Graphics (16 threads)
- kernel: 7.0.0-38-generic
- wasmtime: 47.0.4
- forkasm: 0.1.0, release build
- rustc: rustc 1.99.1 (58cf8f9c3 2026-10-13)

### Fork latency vs memory size, 0.78% of pages dirty (median / p99)

| memory MiB | dirty pages | S0 fork | S1 snapshot | S1 spawn | S1 fork | process fork() |
|---|---|---|---|---|---|---|
| 1 | 1 | 698.4 µs / 1.14 ms | 12.6 µs / 25.1 µs | 33.8 µs / 48.1 µs | 46.6 µs / 73.0 µs | 686.7 µs / 1.26 ms |
| 4 | 5 | 1.75 ms / 3.04 ms | 21.1 µs / 36.9 µs | 33.7 µs / 56.1 µs | 55.0 µs / 95.8 µs | 611.1 µs / 1.12 ms |
| 16 | 29 | 7.58 ms / 14.80 ms | 22.3 µs / 38.0 µs | 34.1 µs / 51.2 µs | 57.0 µs / 91.3 µs | 880.0 µs / 1.51 ms |
| 64 | 125 | 75.82 ms / 81.98 ms | 59.2 µs / 97.4 µs | 41.2 µs / 78.0 µs | 101.4 µs / 146.7 µs | 900.0 µs / 1.55 ms |
| 256 | 509 | 299.73 ms / 310.39 ms | 223.9 µs / 392.2 µs | 51.4 µs / 153.3 µs | 276.8 µs / 492.9 µs | 889.8 µs / 1.72 ms |
| 1024 | 2042 | 1.12 s / 1.15 s | 892.7 µs / 1.18 ms | 53.9 µs / 92.0 µs | 947.4 µs / 1.28 ms | 893.9 µs / 1.54 ms |

- S1 = default configuration (write-protect tracking + layer pool); process fork() = until the child runs
