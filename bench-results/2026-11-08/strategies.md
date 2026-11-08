- cpu: AMD Ryzen 7 7735HS with Radeon Graphics (16 threads)
- kernel: 7.0.0-38-generic
- wasmtime: 47.0.4
- forkasm: 0.1.0, release build
- rustc: rustc 1.99.1 (58cf8f9c3 2026-10-13)

### Fork latency by strategy: 64 MiB, 128 dirty pages (median / p99)

| strategy | iterations | snapshot | spawn | fork |
|---|---|---|---|---|
| S0 (full copy) | 100 | 48.31 ms / 52.38 ms | 26.27 ms / 28.62 ms | 74.74 ms / 79.67 ms |
| S1 pagemap read | 1000 | 274.4 µs / 341.7 µs | 32.3 µs / 51.8 µs | 308.0 µs / 385.4 µs |
| S1 PAGEMAP_SCAN | 1000 | 221.2 µs / 287.3 µs | 32.8 µs / 49.8 µs | 255.2 µs / 330.7 µs |
| S1 scan + pool | 1000 | 93.3 µs / 172.2 µs | 33.0 µs / 84.6 µs | 127.5 µs / 256.0 µs |
| S1 write-protect | 1000 | 196.8 µs / 286.5 µs | 40.1 µs / 72.8 µs | 237.3 µs / 341.9 µs |
| S1 write-protect + pool (default) | 1000 | 58.0 µs / 106.7 µs | 40.4 µs / 78.0 µs | 98.9 µs / 161.7 µs |
| S2 lazy (userfaultfd) + pool | 1000 | 59.4 µs / 123.5 µs | 14.1 µs / 36.5 µs | 74.2 µs / 145.3 µs |
| process fork() (child ready) | 200 | – | – | 627.0 µs / 1.18 ms |
| process fork() + 1-page write, reply | 200 | – | – | 727.4 µs / 1.29 ms |

- process fork() copies the page tables of the whole host process (here holding one 64 MiB sandbox)
