- cpu: AMD Ryzen 7 7735HS with Radeon Graphics (16 threads)
- kernel: 7.0.0-38-generic
- wasmtime: 47.0.4
- forkasm: 0.1.0, release build
- rustc: rustc 1.99.1 (58cf8f9c3 2026-10-13)

### Fork latency vs dirty pages, 64 MiB (median / p99)

| dirty pages | dirty % | S0 fork | S1 snapshot | S1 spawn | S1 fork | S1 maps/child |
|---|---|---|---|---|---|---|
| 1 | 0.01 | 72.95 ms / 78.15 ms | 13.5 µs / 21.2 µs | 33.3 µs / 51.3 µs | 47.0 µs / 92.0 µs | 4 |
| 16 | 0.10 | 73.33 ms / 78.16 ms | 18.4 µs / 37.0 µs | 33.8 µs / 49.2 µs | 52.3 µs / 75.9 µs | 4 |
| 161 | 1.00 | 73.92 ms / 80.91 ms | 74.2 µs / 136.5 µs | 40.4 µs / 89.9 µs | 116.4 µs / 192.7 µs | 4 |
| 1612 | 10.00 | 79.30 ms / 89.66 ms | 794.4 µs / 1.27 ms | 81.0 µs / 133.0 µs | 876.2 µs / 1.37 ms | 4 |
| 8064 | 50.00 | 76.20 ms / 88.33 ms | 13.16 ms / 14.57 ms | 87.3 µs / 124.1 µs | 13.25 ms / 14.66 ms | 4 |
| 16128 | 100.00 | 74.67 ms / 77.56 ms | 26.20 ms / 28.29 ms | 79.4 µs / 124.4 µs | 26.28 ms / 28.36 ms | 4 |

- dirty pages are contiguous; S1 = default configuration
