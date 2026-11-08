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
[ok  ] uffd WP_ASYNC on shmem (DirtyTracking::WriteProtect): available
```

Consequences:
- `PM_FILE` works as SPEC §5.3 assumes, and `PAGEMAP_SCAN` is available (kernel ≥ 6.7). S1 defaults to `PAGEMAP_SCAN`; reading `/proc/self/pagemap` directly is kept as a fallback and cross-check.
- Unprivileged userfaultfd is disabled, but `UFFD_USER_MODE_ONLY` works, so S2 is possible.
- `vm.max_map_count` is **1,048,576** here, not the 65,530 default SPEC §5.3 quotes. The VMA-limit test (§7) therefore can't rely on hitting the real cap. Proposal: a configurable VMA budget in `Config`, tested with a low value, plus measuring the real cap separately.
- The untouched page in the probe shows up as present + file because of fault-around. That's harmless, since only present-and-not-file pages count as dirty.

## Decisions and findings

### 2026-11-08 — Final: docs, full benchmark run, compile cache, faster rewrite, crate metadata
- **The full benchmark suite has been run:** `bench-results/2026-11-08/`, written up in `BENCHMARKS.md` with charts (`scripts/charts.py` → `docs/charts/*.svg`). The whole suite takes ~3 min, not the hours feared.
  - Headline: S1 default fork 98.9 µs / 161.7 µs, S2 74.2 µs, process fork 627 µs, S0 74.7 ms.
  - The < 50 µs target is **still missed**. Spawn (~40 µs) is the floor.
  - Idle-branch memory is < 1 KiB of PSS (8 VMAs; kernel memory not counted), and 1,000 live children work. Those targets are met.
- **`Config::compile_cache`** (`CompileCache::{Off, UserDefault, Dir}`) turns on Wasmtime's on-disk cache. It is off by default in the library. The CLI uses `UserDefault`, and CPython tests use a cache under `CARGO_TARGET_TMPDIR`.
- **The rewrite pass is now section-level** (`wasmparser` + `wasm-encoder` 0.252, the versions Wasmtime already builds). walrus is gone.
  - The code section is copied verbatim and scanned in parallel for table/passive-data instructions. The export section is copied with the new exports appended.
  - CPython's rewrite: 2.0 s → 45 ms.
  - With the cache, `Runtime::load` of CPython takes 3.4 s → ~1.0 s cold and ~90 ms cached. Debug `cargo test` takes 8 min → ~1m40s.
  - Behaviour is unchanged: same globals, image and start handling, and the same rejections. Invalid modules are now caught by Wasmtime's validation at compile time, rather than by walrus's parse.
- **New clippy** (rustc 1.99): `chunks_exact(32)` → `as_chunks::<32>()` in `mem/uffd.rs`.
- **Crate prep:**
  - LICENSE-MIT and LICENSE-APACHE added.
  - `forkasm` gained `readme`, `keywords`, `categories`, `include` and docs.rs targets, with README and licenses symlinked into the crate.
  - The name `forkasm` is free on crates.io.
  - `cargo publish --dry-run` passes. **Not published:** that needs the owner's crates.io token and is irreversible.
- **SPEC.md §14** lists every adopted deviation in one place.
- **Wasmtime is still pinned to 47.0.4,** although rustc 1.99 would now allow 48+. Upgrading is a separate change; re-run the suite after it.

### 2026-10-11 — Week 3: G2 (CPython), branch driver, Baseline B, S2, benchmark suite
Started early (Week 3 is scheduled for Oct 24–30), with the user's approval. Per the user, the full benchmark suite is **written and smoke-tested but not run**; full runs come later (commands under Benchmarks).

**G2 — CPython 3.12 (`guests/py`).** It worked on the first build, so there was no need for the QuickJS fallback.
- `guests/py/build.sh` downloads wasi-sdk 20 and VMware Labs' prebuilt `libpython3.12.a` (built for wasi-sdk 20) into `~/.cache/forkasm-toolchain`, checks their sha256, and links `driver.c` as a WASI reactor.
  - The driver exports `py_init`, `py_exec(ptr, len)`, `py_get_int(ptr, len)` and `py_alloc`/`py_free`.
  - Output: `guests/py/out/py.wasm` (19 MB; 4.2 MB of data segments, 4.6 MB initial memory) and `out/lib` (the stdlib as `python312.zip` plus the `os.py` landmark), mounted read-only at `/usr/local/lib`.
- `py_init` uses an isolated config: no `site` import, unbuffered stdio, no signal handlers, and a fixed hash seed so branches replay identically.
- CPython hit **no unimplemented WASI call**. The rewrite pass handles it as is: 1 mutable global, 2 data segments, no start function.
- `forkasm::python` wraps the exports for tests, the CLI and the driver. Tests (`tests/python.rs`) run under every strategy:
  - forks between statements;
  - an exception doesn't poison the sandbox;
  - stdlib imports, files and read-only mounts;
  - deterministic replay;
  - S0 equivalence after a statement sequence with forks.
- **Finding: `random` keeps its state across forks.** The module seeds itself at import, so branches forked after `import random` continue the same sequence (as with `os.fork`). `os.urandom` reads the per-branch WASI RNG, and so does `random.seed()` with no argument after the fork. The driver demo reseeds in each branch.
- Costs:
  - Compiling the 19 MB module with Cranelift takes ~3.4–4 s per `Runtime::load`. Wasmtime's compilation cache would remove this; left for the polish week.
  - Root spawn plus `py_init`: ~22 ms.

**CPython's dirty pattern (`forkasm-cli py-profile`), and tiered compaction.**
- A small statement (`x += 1`) dirties ~190 KiB (~47 pages) in ~50 separate runs across the heap and stack. Bigger statements dirty 0.5–1 MiB, and `bytearray(8 MiB)` dirties 8 MiB.
- Snapshots stay cheap (50–150 µs). **Spawn dominates**: each statement adds ~50 runs, and a spawn costs ~3–4 µs per `mmap`. 150–280 mappings cost ~0.6–1 ms, while a spawn from a compacted snapshot (4 mappings) is 24 µs. So spawn cost is the kernel's `mmap` per segment.
  - Instantiation itself is cheap. Checked with strace: the many `mprotect`s are glibc arena growth during compilation.
- On an uncompacted chain, the first spawn of a snapshot also pays for building the mapping plan: ~430 µs at 300 layers. Siblings reuse it.
- **Tiered compaction is implemented** (`sandbox.rs`, `merge_point`). When a snapshot exceeds a threshold, only the newest layers merge with the dirty set into one layer (the gap-filled union of their pages, copied from memory). The rule:
  - merge the fewest layers such that the layer below is ≥ 2× the merged size (`TIER_RATIO`), so sizes grow geometrically towards the bottom;
  - the resulting stack must be within **half** of both thresholds (hysteresis, so the next merge is some snapshots away);
  - if nothing qualifies, everything is flattened, as before.

  `Sandbox::snapshot_compacted` still flattens everything.
  - Without the hysteresis, merges stopped just under `max_runs`, and the kv chain ended at 505 mappings per child instead of 131.
- Results:
  - kv adversarial chain (`bench-depth`): the default is unchanged (51 merges, 131 maps/child, spawn 845 µs / 1.74 ms). Its p99 snapshot (~67 ms) is still a full-memory copy, because random pages across 64 MiB make every merge cover everything.
  - CPython chain (300 levels of small statements, `py-profile`):

    | compaction | snapshot | spawn | fork |
    |---|---|---|---|
    | off | 74 µs | 885 µs | 961 µs |
    | default | 63 µs | 367 µs | 432 µs |
    | `max_runs` 64 | 322 µs | 213 µs | 442 µs |
    | `max_runs` 32 | 1.01 ms | 45 µs | 1.05 ms |

    Values are medians. The p99 snapshot is ~1.3 ms everywhere (the largest merge).
- **Decision:** keep the defaults (≤ 32 layers, ≤ 512 mappings, gap 16). For fan-out-heavy use (one snapshot, many spawns), `max_runs` 64 halves spawn time at equal fork cost. A real fix for spawn cost needs fewer `mmap`s per spawn, not compaction.
- 2,000 property cases pass with tiered merging under the tight-compaction variants (207 s).

**Branch driver (`driver.rs`) and `Runtime::run_parallel` (SPEC §5.6–5.7), on `rayon`.**
- `run_parallel(sandboxes, f)` returns `Vec<(Sandbox, R)>` in input order.
  - **Deviation from the spec sketch:** it doesn't return `Result`. `f` returns whatever it likes (often a `Result`), so one failed branch doesn't fail the batch.
- `driver::search(root, Search { branching, keep, depth }, step)` is a beam search:
  1. Each level spawns `branching` children of every survivor, in parallel, inside the pool tasks.
  2. It runs `step`, which returns an `f64` score. Branches with an error or a NaN score are dropped.
  3. It keeps the top `keep` (stable, so ties keep tree order) and snapshots them in parallel.

  A child's RNG branch is its index under its parent, so the same path replays identically.
- `tests/driver.rs`:
  - `run_parallel` keeps order and isolation;
  - the parallel search equals a sequential reference and is deterministic, including survivor memory;
  - a search where every branch fails ends early;
  - a CPython beam search converges on a target.
- `Sandbox` turned out to be `Send` and `Snapshot` `Sync` already. The thread-local slot in `mem/creator.rs` is set and taken on the same thread within one instantiation, so it is safe under rayon.

**Baseline B — process `fork()` (`mem/procfork.rs`, `Sandbox::process_fork_call`).**
- Forks the host process. The child calls one export on its copy of the sandbox, writes the result to a pipe, and `_exit`s. The parent times three points: `fork()` returning, the child running, and the result.
- Only the calling thread survives in the child. That is fine here: running guest code needs none of forkasm's helper threads, and glibc malloc has its own fork handlers. Benchmark-only.
- Smoke number: ~0.8–1.1 ms until the child runs, for a host holding one 64 MiB sandbox (page-table copy of the whole process).

**S2 — lazy userfaultfd (`mem/lazy.rs`, `Strategy::Lazy`).**
- A spawn makes `[0, len)` plain anonymous memory and registers the whole reservation on the process-wide userfaultfd, for both `MISSING` and `WP` (same fd as write-protect tracking, `UFFD_USER_MODE_ONLY`).
- A single handler thread fills each missing page from the snapshot the memory was spawned from. It finds the newest layer holding the page through the snapshot's plan, otherwise the base image, using `pread` + `UFFDIO_COPY` with `MODE_WP`.
- A page that was only read is therefore write-protected and not dirty, and snapshots reuse S1's write-protect scan and layer capture unchanged. S2 always uses write-protect tracking.
- **Untouched pages keep their spawn-time contents forever**, so a memory's spawn source stays valid after it takes snapshots of its own. Compaction therefore never remaps an S2 memory: `compose` is a no-op for it.
- Pitfalls handled:
  - With `UFFD_USER_MODE_ONLY`, a kernel-mode read of a missing page fails with `EFAULT`, and layers too big for a slab are written with `pwritev`. `capture_layer` therefore pre-touches an S2 memory's runs from user mode (it matters for compaction, which copies clean pages).
  - The handler holds the registry lock from lookup to `UFFDIO_COPY`. Otherwise a stale duplicate fault could fill a page of a *new* memory mapped at a reused address with the old memory's contents.
  - Zero pages are filled with `UFFDIO_COPY` + WP too: a plain zero page would not be write-protected, so writes to it would go unreported.
  - `process_fork_call` refuses S2: the child process doesn't inherit the registration, so unfilled pages would read as zeros.
- Tested everywhere S0/S1 are: `strategies()` and `layered_configs()` include S2 (default and tight compaction) when the kernel supports it, so the oracle, isolation, filesystem, CPython, driver, leak and property tests all run it.
- Smoke numbers, quick run only (~5 iterations):
  - fork ≈ S1 (spawn is O(1) but the snapshot is the same);
  - each first touch costs ~12 µs (a round trip through one handler thread), so a child that writes 768 pages takes ~10 ms against ~3 ms for S1;
  - the CPython beam search on 16 threads is ~15% slower than S1.

**Captured output is now shared across forks (`wasi/output.rs`).** stdout/stderr are an append-only `Output`: full 4 KiB chunks live in an `imbl::Vector<Arc<[u8]>>`, and a clone copies at most the unsealed tail. Forking a sandbox that printed a lot is therefore no longer O(output). This closes the Week 2 "known cost".
- `Sandbox::stdout()`/`stderr()` now return `Vec<u8>`.

**Test times:** plain `cargo test` (debug) is green in ~8 min. The G2 tests take ~400 s of that, because debug-built Cranelift compiles CPython's 19 MB module slowly. `cargo test --release` takes ~2 min.

**Property run with S2:** `FORKASM_PROPTEST_CASES=2000` passes all 2,000 cases with S2 among the variants (229 s).

**Benchmark suite (`forkasm-cli bench-suite`, `crates/forkasm-cli/src/suite.rs`).** It writes `<name>.md` and `<name>.csv` per benchmark, with an environment header (CPU, kernel, Wasmtime version from `Cargo.lock`, rustc, build profile):
- `strategies`: every S0/S1/S2 variant plus process fork, at 64 MiB with 128 dirty pages.
- `size`: 1 MiB → 1 GiB at 0.78% dirty, for S0, S1 and process fork.
- `dirty`: 1 page → 100% at 64 MiB.
- `memory`: PSS per child from `smaps_rollup`, plus VMAs per child, at N = 1/10/100/1000, idle and with 16 dirty pages.
- `depth`: the compaction chain.
- `lazy`: S1 vs S2, fork against time to a first result.
- `python`: end-to-end beam search over CPython branches (8 × keep 4 × depth 6), for S1 on 1 thread and on all threads, and S2 on all threads.

`bench-fork` and `bench-depth` now print the `strategies` and `depth` tables. `--quick` smoke-tests everything in ~10 s.

**Demo (`forkasm-cli demo <py.wasm> <stdlib>`).**
1. A CPython root loads a task (sort 20,000 records) and a checker.
2. It forks into 8 branches, one per approach: 6 correct, 1 wrong answer, 1 raising. The snapshot takes ~0.9 ms and spawning all 8 ~1.2 ms, at 7.4 MiB each.
3. The branches run in parallel, the checker scores them, the fastest correct branch continues, and the rest are dropped.

It runs end to end.

### 2026-10-11 — Week 2: VFS and WASI file calls
- **`vfs/`** is a persistent CoW tree. Inodes live in an `imbl::OrdMap<Ino, Inode>`. Directories are `OrdMap<String, Ino>`. Files are `Vec<Option<Arc<[u8; 64 KiB]>>>`, with `None` for holes.
- Cloning a `Vfs` is O(1). After a fork, writing one byte copies one chunk plus the B-tree path to its inode. `vfs::tests::fork_shares_until_written` checks the sharing.
- **Deviation from SPEC §5.5:** the inode table sits between directories and files. The spec sketches directories holding nodes directly. The extra level of indirection gives POSIX semantics to open files: an fd refers to an inode, so it survives rename and unlink, and the inode is freed when the last fd closes. `filesystem::open_files_survive_forks_with_their_own_offsets` tests this.
- **`imbl` instead of `im`.** `im` has had no release since 2022, and `imbl` is its maintained fork with the same API.
- **`wasi/fs.rs`** implements the SPEC §5.4 list, plus `fd_pread/pwrite/tell/renumber/allocate`, `fd_filestat_set_size`, `path_remove_directory`, no-op sync/advise/set-times calls, and `path_readlink`, which returns `EINVAL` because there are no symlinks.
  - fd 3 is the preopen `/`.
  - The fd table (`FdTable`) lives in `WasiState`, so forks clone the offsets.
  - The guest's `std::fs` calls are tested through new G1 exports: `append_file`, `file_hash`, `file_len`, `fs_mkdir`, `fs_remove`, `fs_rename`, `list_count`, `hold_open`, `held_read`, `held_hash`.
- **Read-only host mounts:** `SpawnOpts::mounts` copies a host tree in at root spawn and marks it read-only, so writes return `EROFS`. Contents are `Arc`-shared by every fork.
  - The copy is eager. For CPython's stdlib (Week 3) that's roughly 30 MB, read once per root.
- **Limitations:**
  - **Known cost (fixed in Week 3, see `wasi/output.rs`):** captured stdout/stderr were plain `Vec<u8>`s in `WasiState`, copied by every snapshot.
  - There are no symlinks, and host-mount symlinks are followed at copy time.
  - Timestamps can't be set; mtime is virtual time at modification.
  - `..` can't climb above the fd it is resolved against. This matches WASI capability rules: `/` is the root preopen.

### 2026-10-11 — Week 2: compaction and mapping plans
- **Compaction** (`Config::compaction`, `Sandbox::snapshot_compacted`) triggers at snapshot time when the new stack would exceed either threshold:
  - `max_layers` layers, or
  - `max_runs` mappings per spawn.

  Every page any layer covers is then copied from the parent, which holds exactly the snapshot's contents at that moment, into one new layer. Holes of up to `gap_pages` pages are filled so runs merge. The parent recomposes onto the new layer, which drops its private pages, the old layers' VMAs, and the write-tracking state.
- **Mapping plans (`LayerStack::plan`).** A spawn used to map every run of every layer, oldest first, so pages covered by several layers were mapped several times. Now each snapshot computes once, lazily, the non-overlapping set of segments, each from the newest layer holding it, with adjacent same-layer pieces merged. Spawns `mmap` once per segment.
  - Spawn on a 64-layer chain: ~400 µs → ~170 µs.
  - Compaction also keys off the real segment count.
  - `layer::tests::random_stacks_match_brute_force` checks the painting against a per-page reference on 2000 random stacks.
- **Pool:** one process-wide worker thread now serves every `LayerPool`. Previously each `Runtime` started its own thread, which cost a malloc arena each.
- **S0 spawn** pre-faults with `MADV_POPULATE_WRITE` before copying: 2.3 ms → 0.66 ms for ~3–4 MiB. That makes S0 a fairer baseline, and the property tests faster.
- **G1 links with a 64 KiB stack** (`guests/kv/.cargo/config.toml`) instead of Rust's default 1 MiB. S0 copies every page of memory on each fork.

### 2026-10-11 — Week 2: compaction trade-off, and defaults tightened (proposal: tiered compaction)
`forkasm-cli bench-depth` (see Benchmarks) shows the trade-off on an adversarial workload: 64 *random* pages per level, across a 63 MiB buffer, for 256 fork-of-fork levels.
- **Without compaction**, a deep child needs ~14k mappings. Spawn costs 37 ms (~2.6 µs per `mmap`), and the snapshot scan is slow too (2.6 ms, walking ~14k VMAs).
- **The old defaults (≤ 64 layers, ≤ 4,096 mappings)** still left 3,237 mappings per child, so 1,000 branches would exceed the VMA cap. New defaults: **≤ 32 layers, ≤ 512 mappings, gap 16**. Spawn median 0.86 ms, 131 mappings per child.
- **Compaction is O(pages covered by any layer).** With scattered writes and gap filling, the flat layer soon covers the whole buffer, so each compaction is a ~64 MiB copy: ~68 ms, the p99 of every compacting configuration. It ran 51 times in 256 levels, a heavy amortized cost for this access pattern.
- **Proposal:** tiered (LSM-style) compaction. Merge only the newest layers when they exceed the thresholds, and fold into older tiers geometrically, so each page is copied O(log n) times instead of on every full flatten. Leave the decision for Week 3, after seeing CPython's real dirty pattern: CPython's heap is likely far more clustered than this benchmark.

### 2026-10-11 — Week 2: correctness suite
- **Property tests** (`tests/proptest_tree.rs`) run random sequences of {guest work, host write, guest file op, snapshot, compacted snapshot, spawn, fork-of-fork chain of 1–40 levels, drop sandbox, drop snapshot}. Each sequence runs on a sandbox tree under S0 and one randomly chosen S1 variant, side by side. The S1 variants are each tracking mode, without a pool, and with tight compaction (`max_layers` 4, `max_runs` 24).
  - After every step, the touched pair must match in memory, globals, stdout, RNG, clock, filesystem and fd count. At the end, a sample of snapshots is respawned and compared.
  - The run is split into 8 shards that run in parallel.
  - `FORKASM_PROPTEST_CASES=10000 cargo test --release -p forkasm --test proptest_tree`; plain `cargo test` runs 24 cases (debug) or 256 (release). Don't use `PROPTEST_CASES`: proptest applies it to *each* shard, which multiplies the total by 8.
- **Result:** `FORKASM_PROPTEST_CASES=10000` passes all 10,000 cases in 993 s (release, 8 shards). 1,765 cases built trees deeper than 64 levels; the deepest reached 134. An earlier, accidentally 8× larger run had already passed 14,039 cases with no failure before it was stopped.
- **Leak test** (`tests/leaks.rs`, its own binary): busy trees under every strategy and variant, then drop everything. Result: memfds return to 0, VMAs return exactly to baseline, and `RssShmem` returns to 0.
  - `RssAnon` creeps by ~0.2 MB per *runtime* created and levels off around 28 MB. Creating and dropping 1,600 sandboxes and snapshots on one runtime is flat.
  - The creep therefore comes from Wasmtime engine and compile allocations plus allocator arenas, not forkasm. The test allows 32 MiB of anon slack.
- **VMA-limit test** (`tests/limits.rs`, its own binary). Exhausting `vm.max_map_count` makes *every* `mmap` in the process fail (allocator, Wasmtime), and those abort instead of erroring. So the uncompacted case is measured and extrapolated:
  - A snapshot whose 4096 dirty pages alternate costs 8,199 VMAs per uncompacted child, so 192 children would need 1.57 M against the 1,048,576 cap.
  - With compaction (`max_runs` 1024), the same 192 children use 4 VMAs each and have byte-identical memory.

### 2026-10-11 — Implemented both fork-latency proposals: 224 µs → 95 µs median fork
Both proposals from the entry below are now in (`mem/pool.rs`, `mem/uffd.rs`). Default config = `DirtyTracking::Auto` (→ `WriteProtect` when available) + `layer_pool: Some(16 MiB × 2)`. Snapshot breakdown with both: scan ~19 µs, `memcpy` of 512 KiB ~40 µs. Spawn ~33–37 µs is now the other half of the fork, and the next target.

**Layer pool (`mem/pool.rs`).**
- A background thread keeps `ready_slabs` memfds ready. Each is `fallocate`d, then mapped shared and pre-faulted.
- A snapshot carves its layer from the current slab with a bump pointer and `memcpy`s the dirty runs in, *packed*. A layer's runs are mapped from consecutive file offsets that differ from their memory offsets.
- Layers bigger than a slab get their own memfd, filled with `pwritev`. The first snapshot of a big root goes this way.
- A dropped layer's range is hole-punched by the worker. A retired slab's unused tail is punched too. A slab closes when its last layer drops.
- `Runtime::layer_pool_misses()` counts snapshots that found no slab ready. There were 0 in the benchmarks.
- `Runtime::warm_up()` fills the pool synchronously.

**Write-protect tracking (`mem/uffd.rs`).**
- One process-wide userfaultfd, using `UFFD_USER_MODE_ONLY | WP_ASYNC | WP_HUGETLBFS_SHMEM`. A `MAP_PRIVATE` memfd mapping counts as shmem.
- Each S1 memory registers its whole reservation after composing, so pages added by `memory.grow` are covered.
- The dirty scan is `PAGEMAP_SCAN` with `PM_SCAN_WP_MATCHING | PM_SCAN_CHECK_WPASYNC`, requiring `WRITTEN && !FILE && (PRESENT || SWAPPED)`. File pages are never reported. Anonymous pages are reported once per write and re-protected atomically.
- The parent therefore keeps its private pages and skips the remap. The cost is that it holds an O(dirty) private copy of what it wrote.

Things learned along the way:
- **A layer must stay alive while it is mapped.** A plain memfd stays alive because the mapping holds the file. A slab range, however, is hole-punched when its layer drops, and `MAP_PRIVATE` pages that were never copied then read as zeros. `Mapping` now pins every layer it maps (`pinned`). Regression tests: `mem::tests::pool_packs_layers_and_reuses_slabs` and `isolation::children_survive_dropping_ancestors`.
- **Write-protect scans are destructive.** Pages reported but not captured are lost to the next snapshot. `Sandbox` therefore captures right after every scan, and a failed layered snapshot *poisons* the sandbox instead of risking a silently incomplete next snapshot.
- **Don't use `MAP_POPULATE` off-thread.** The first pool version pre-faulted slabs with `MAP_POPULATE`. That holds the process-wide `mmap_lock` for the whole 16 MiB, and every concurrent `mmap` (each spawn) stalled behind it: p99 fork was 6.3 ms. Touching one byte per page from the worker uses per-VMA-locked faults instead, and p99 dropped to 162 µs.
- The pool and write-protect are independent. Pool alone: 133 µs; write-protect alone: 231 µs; both: 95 µs. The oracle tests run every combination against S0.

### 2026-10-11 — S1 fork is ~225 µs, not < 50 µs: where the time goes (proposals — since implemented, see above)
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
| `wasmparser`, `wasm-encoder` (=0.252, Wasmtime's own versions) | Section-level rewrite pass; replaced `walrus` (2026-11-08), which rebuilt every function body |
| `wasmtime` feature `cache` | On-disk compile cache (`Config::compile_cache`); pulls `zstd` and `toml` |
| `anyhow`, `thiserror` | Error handling |
| `wat` (dev) | Inline test modules |
| `criterion` (dev) | Micro-benchmarks (SPEC §8) |
| `imbl` | Persistent maps for the VFS (SPEC §5.5's `im::OrdMap`; `imbl` is the maintained fork) |
| `proptest` (dev) | Property tests against S0 (SPEC §7) |
| `rayon` | Parallel branches for `run_parallel` and the branch driver (SPEC §5.7) |

## Benchmarks

### 2026-11-08 — Full suite
See `BENCHMARKS.md` and `bench-results/2026-11-08/`. The command to rerun it is in `BENCHMARKS.md`.

### 2026-10-11 — Week 2: compaction trade-off (`forkasm-cli bench-depth … 256`)
Same machine as below. A fork-of-fork chain of 256 levels on 64.1 MiB of memory. Each level dirties 64 random pages, snapshots, and continues in a new child. Default S1 (write-protect + pool). Median / p99:

| compaction | snapshot | spawn | compactions | maps per child at the end |
|---|---|---|---|---|
| off | 2.61 ms / 4.15 ms | 36.83 ms / 52.84 ms | 0 | 14,023 |
| layers ≤ 8 | 230.5 µs / 68.08 ms | 1.58 ms / 2.88 ms | 32 | 1 |
| layers ≤ 32 | 504.4 µs / 72.03 ms | 6.13 ms / 12.00 ms | 8 | 1 |
| layers ≤ 128 | 1.41 ms / 3.06 ms | 22.45 ms / 36.91 ms | 2 | 1 |
| maps ≤ 1024 | 240.7 µs / 68.92 ms | 1.64 ms / 3.66 ms | 28 | 510 |
| **default (layers ≤ 32, maps ≤ 512, gap 16)** | **189.5 µs / 67.75 ms** | **859.1 µs / 1.75 ms** | 51 | 131 |

The p99 snapshot (~68 ms) of every compacting configuration is the full flatten.

The headline benchmark is unchanged by Week 2 (`bench-fork … 1000`): S1 write-protect + pool forks in **93.4 µs / 159.5 µs**. S0 spawn dropped from 31.5 to 25.8 ms with pre-faulting.

### 2026-10-11 — Layer pool + write-protect tracking
Same setup as the Week 1 entry below. `forkasm-cli bench-fork … 1000` (median / p99):

| strategy | snapshot | spawn | fork |
|---|---|---|---|
| S0 | 47.69 ms / 51.02 ms | 31.48 ms / 32.90 ms | 79.21 ms / 82.78 ms |
| S1 pagemap, remap | 288.2 µs / 470.5 µs | 30.8 µs / 76.1 µs | 319.1 µs / 537.6 µs |
| S1 scan, remap | 234.5 µs / 322.6 µs | 29.4 µs / 63.8 µs | 266.5 µs / 358.0 µs |
| S1 scan + pool | 99.9 µs / 191.3 µs | 31.3 µs / 87.6 µs | 132.6 µs / 262.6 µs |
| S1 write-protect | 196.3 µs / 283.9 µs | 32.8 µs / 64.3 µs | 230.9 µs / 327.2 µs |
| **S1 write-protect + pool (default)** | **59.8 µs / 120.0 µs** | **33.9 µs / 77.7 µs** | **95.2 µs / 162.2 µs** |

Criterion medians for `snapshot+spawn`:
- S0: 79.5 ms
- S1-pagemap: 314.7 µs
- S1-scan: 264.0 µs
- S1-scan+pool: 134.9 µs
- S1-wp: 231.1 µs
- S1-wp+pool: 99.6 µs

The default S1 fork is ~830× faster than S0. It is still ~2× the 50 µs target; spawn (~34 µs) is now half of it.

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
