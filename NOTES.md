# forkasm — working notes

Newest entries first within each section.

## Environment (`scripts/envcheck.sh`, 2026-10-11)

```
kernel:                   7.0.0-38-generic
cpu:                      16 threads
vm.max_map_count:         1048576
unprivileged_userfaultfd: 0
rustc:                    1.94.1
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

_None yet._
