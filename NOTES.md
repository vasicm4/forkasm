# forkasm — working notes

Newest entries first within each section.

## Environment (`scripts/envcheck.sh`, 2026-10-11)

```
kernel:                   7.0.0-38-generic
cpu:                      AMD Ryzen 7 7735HS with Radeon Graphics (16 threads)
vm.max_map_count:         1048576
unprivileged_userfaultfd: 0
shmem THP:                never
rustc:                    1.94.1 (e408947bf 2026-03-25)
wasmtime:                 47.0.4

[ok  ] memfd_create: created a sparse 4 GiB memfd
[ok  ] pagemap PM_FILE on MAP_PRIVATE memfd: untouched/read/written/read+written = [false, false, true, true]; flag bits 55-63: ["0x143", "0x143", "0x103", "0x103"]
[ok  ] PAGEMAP_SCAN ioctl: dirty runs [Run { page: 2, len: 2 }]
[ok  ] userfaultfd(UFFD_USER_MODE_ONLY): available (user-mode faults only)
```

Consequences:
- `PM_FILE` works as SPEC §5.3 assumes, and `PAGEMAP_SCAN` is available (kernel ≥ 6.7). S1 defaults to `PAGEMAP_SCAN`; reading `/proc/self/pagemap` directly is kept as a fallback and cross-check.
- Unprivileged userfaultfd is disabled, but `UFFD_USER_MODE_ONLY` works, so S2 is possible.
- `vm.max_map_count` is **1,048,576** here, not the 65,530 default SPEC §5.3 quotes. The VMA-limit test (§7) therefore can't rely on hitting the real cap. Proposal: a configurable VMA budget in `Config`, tested with a low value, plus measuring the real cap separately.
- The untouched page in the probe shows up as present + file because of fault-around. That's harmless, since only present-and-not-file pages count as dirty.

## Decisions and findings

### 2026-10-11 — S1 fork is ~225 µs, not < 50 µs: where the time goes (proposals for Week 2)
This is a temporary per-phase instrumentation of `Sandbox::snapshot` (since reverted). The setup is 64 MiB of memory, 128 dirty pages (one run), and `PAGEMAP_SCAN`:

| phase | time |
|---|---|
| dirty scan (`PAGEMAP_SCAN`) | ~14 µs |
| capture: `memfd_create` + `ftruncate` | ~4 µs |
| capture: allocate 128 shmem pages | ~130 µs (≈1 µs/page; measured with `fallocate` alone) |
| capture: copy 512 KiB | ~40 µs |
| parent remap over the new layer (`mmap MAP_FIXED`, frees 128 anon pages) | ~37 µs |
| spawn (instantiate + compose 1–2 layers + restore globals) | ~31 µs |

So the critical path is dominated by **shmem page allocation**, not by scanning or copying. Two proposals:

1. **Pre-allocated, packed layer pool.**
   - Store a layer's pages packed (file offset ≠ memory offset; `mmap` allows this) in memfds that were `fallocate`d ahead of time.
   - Refill the pool off the fork path, for example between calls or on a helper thread.
   - Use one `pwritev` per snapshot.
   - Expected effect: removes the ~130 µs allocation from the fork.
2. **Async write-protect dirty tracking instead of the parent remap.**
   - Register the memory with userfaultfd using `UFFD_FEATURE_WP_ASYNC` (kernel ≥ 6.7; works with `UFFD_USER_MODE_ONLY`).
   - Scan with `PAGEMAP_SCAN` + `PM_SCAN_WP_MATCHING`, which reports *written since last scan* and re-protects atomically.
   - The parent then keeps its private pages, and the ~37 µs remap disappears. The cost is that the parent holds a private copy of the pages it dirtied (O(dirty)).

S0 is 78 ms per fork, so S1 is already ~350× faster. The < 50 µs target is not met yet.

### 2026-10-11 — Wasmtime pinned to 47.0.4
Wasmtime 48/49 need rustc ≥ 1.95/1.96, and the installed toolchain is 1.94.1. 47.0.4 has the same `MemoryCreator`/`LinearMemory` API. Upgrading means `rustup update` plus a version bump.

### 2026-10-11 — Custom LinearMemory prototype: works (SPEC §5.2 confirmed)
Wasmtime 47 API:
- `MemoryCreator::new_memory(ty, minimum, maximum, reserved_size_in_bytes, guard_size_in_bytes)`
- `LinearMemory { byte_size, byte_capacity, grow_to, as_ptr }`

Design (`crates/forkasm/src/mem/`):
- Reserve `reservation + guard` as `PROT_NONE`.
- For S1, map the base image (a sparse memfd as large as the whole reservation) `MAP_PRIVATE` over the reservation, inaccessible. Then compose the layers over `[0, size)` read-write. `grow_to` = `mprotect`.
- The mapping is `Arc`-shared between Wasmtime's `Region` and the host's `MemHandle`, so the host can remap pages without depending on Wasmtime's drop order.
- The initial contents travel through a thread-local slot (`instantiate_with`), as the spec planned.
- Verified by `mem::tests`:
  - Wasmtime accepts an initial `byte_size` larger than the module minimum, so a spawn can start at the snapshot's size.
  - Growth past the snapshot size yields zero pages.
  - Out-of-bounds accesses still trap.
  - Children and parent are isolated after a layered spawn.
  - Both dirty scanners agree.
- `Config::memory_init_cow(false)` is **required**. With it on, Wasmtime tries to CoW-map its own image into "mmap-based" memories, and a host memory hits `unreachable!` there (`runtime/vm/memory.rs`, `LocalMemory::new`).

### 2026-10-11 — SPEC deviation (proposed): the rewrite strips active data segments
Wasmtime applies active data segments itself *after* `new_memory` returns. Every spawn would then overwrite the restored snapshot and pay O(data size). Confirmed by `mem::tests::data_segments_clobber_restored_memory`.

**Proposal:** the rewrite pass (§5.1) also removes active data segments and returns them as the module's memory image. The host writes that image into the base layer `L0` (S1), or into the initial bytes (S0), once at load time. Passive segments (`memory.init` / `data.drop`) are rejected, because `data.drop` state can't be captured.

### 2026-10-11 — SPEC deviation (proposed): start function runs only at the root
A `start` section, and a reactor's `_initialize`, would rerun on every spawn and mutate restored state. The rewrite removes `start` and records it. The host runs it (then `_initialize`, if exported) only when spawning a root sandbox.

### 2026-10-11 — Dirty rule includes swapped pages
`dirty = (present || swapped) && !file`. A dirtied anonymous page that has been swapped out is still dirty. `PAGEMAP_SCAN` uses the same rule: inverted `PAGE_IS_FILE` mask, any of `PRESENT | SWAPPED`.

### 2026-10-11 — Layout deviation
Integration tests and criterion benches live in `crates/forkasm/{tests,benches}`, which is the Cargo workspace convention. They are not at the repo root as SPEC §9 shows.

## Dependencies

| Crate | Why |
|---|---|
| `wasmtime` 47 | The runtime; `cranelift` + `runtime` + `std` only (no WASI crates, SPEC §5.4) |
| `rustix` | Safe-typed mmap / memfd / mprotect / userfaultfd syscalls for `mem/` |
| `libc` | `ioctl(PAGEMAP_SCAN)`, which rustix doesn't wrap |
| `walrus` | Wasm rewrite pass (export mutable globals, strip data segments) |
| `anyhow`, `thiserror` | Error handling |
| `wat` (dev) | Inline test modules |
| `criterion` (dev) | Micro-benchmarks (SPEC §8) |

## Benchmarks

### 2026-10-11 — Week 1: fork latency, S0 vs S1, 64 MiB, 128 dirty pages (0.78%)
- Machine: AMD Ryzen 7 7735HS (16 threads), kernel 7.0.0-38-generic, Wasmtime 47.0.4, rustc 1.94.1, release build.
- Guest: G1 (`guests/kv`) with its 63 MiB page buffer fully touched, then snapshotted. Every iteration spawns a parent from that clean snapshot and dirties 128 contiguous pages. Then it times `snapshot()` + one `spawn_from()`.
- "fork" = snapshot + spawn.

`forkasm-cli bench-fork guests/kv/target/wasm32-wasip1/release/kv.wasm 1000` (median / p99, 1000 iterations; S0 100):

| strategy | snapshot | spawn | fork |
|---|---|---|---|
| S0 (full copy) | 47.33 ms / 51.62 ms | 31.38 ms / 34.36 ms | 78.83 ms / 84.65 ms |
| S1, `/proc/self/pagemap` read | 245.0 µs / 352.6 µs | 31.3 µs / 49.5 µs | 277.1 µs / 392.2 µs |
| S1, `PAGEMAP_SCAN` | 192.3 µs / 264.9 µs | 30.5 µs / 53.7 µs | **223.6 µs / 300.0 µs** |

`cargo bench -p forkasm --bench fork` (criterion medians; they agree with the table above):
- `snapshot+spawn`: S0 78.5 ms, S1-pagemap 282.0 µs, S1-pagemap_scan 231.0 µs
- `snapshot`: S0 47.5 ms, S1-pagemap 251.2 µs, S1-pagemap_scan 199.8 µs
- `spawn` from a fixed snapshot: S0 36.2 ms, S1 31.9–32.1 µs
