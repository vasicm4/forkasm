# forkasm

[![CI](https://github.com/vasicm4/forkasm/actions/workflows/ci.yml/badge.svg)](https://github.com/vasicm4/forkasm/actions/workflows/ci.yml)

Fork a **running** WebAssembly sandbox — its memory, WASI state and
filesystem — in about a tenth of a millisecond, many ways and many levels
deep, so an AI agent can branch mid-task, explore alternatives in parallel,
and keep the winner.

```rust
use forkasm::{Config, Runtime, SpawnOpts, Val};

let rt = Runtime::new(Config::default())?;
let module = rt.load("guest.wasm")?;              // rewrite + compile once
let mut root = rt.spawn(&module, SpawnOpts::default())?;
root.call("init", &[])?;

let snap = root.snapshot()?;                      // O(pages dirtied since the last snapshot)
let kids = rt.spawn_many(&snap, 8)?;              // copy-on-write children

let results = rt.run_parallel(kids, |i, sb| sb.call("try", &[Val::I32(i as i32)]));
// Keep the best branch; dropping the rest frees their private pages and layers.
```

Linux only, x86-64, single-threaded guests, Rust 1.94+.

## Headline numbers

Median latency on an AMD Ryzen 7 7735HS laptop with Linux 7.0 and
Wasmtime 47, for a 64 MiB sandbox with 128 pages (0.78%) written since its
last snapshot:

| | fork (snapshot + spawn) | vs forkasm |
|---|---|---|
| **forkasm S1** (default: copy-on-write layers) | **99 µs** (p99 162 µs) | — |
| forkasm S2 (lazy userfaultfd) | 74 µs, then ~12 µs per first touch | |
| process `fork()` of the host | 627 µs | 6.3× slower |
| S0 (copy all memory) | 74.7 ms | 756× slower |
| VM branching: forkd, Morph Infinibranch (*published*, other hardware) | ~150–250 ms | ~1,500–2,500× slower |

- **Idle branches are almost free.** An idle branch adds < 1 KiB of PSS
  (8 VMAs). 1,000 live branches that each wrote 16 pages cost 68 KiB each.
- **Fork time scales with what you wrote, not memory size.** At 1 GiB, a
  spawn is still 54 µs.
- **The CPython guest forks between statements in under 1 ms.**
- **The spec target was < 50 µs, and that is missed by ~2×.** Spawn alone
  takes ~40 µs.

Full tables, charts and methodology: [`BENCHMARKS.md`](BENCHMARKS.md).

## How it works

A sandbox may be forked only **between top-level calls** into the guest, when
no Wasm frames are on the stack. Then the whole state is: one linear memory,
the mutable globals, the WASI state (fd table, args, env, clock, RNG) and a
virtual filesystem. forkasm owns all four.

```
 load time                          snapshot() = O(dirty pages)          spawn = a few mmaps
 ─────────                          ───────────────────────────          ───────────────────
 guest.wasm                          parent memory (MAP_PRIVATE)          child memory
   │ rewrite:                         ┌──┬──┬──┬──┬──┬──┬──┬──┐           ┌──┬──┬──┬──┬──┬──┬──┬──┐
   │  • export mutable globals        │  │▓▓│  │  │▓▓│▓▓│  │  │  dirty    │L0│L1│L0│L0│L2│L2│L0│L0│
   │  • strip data segments ──┐       └──┴──┴──┴──┴──┴──┴──┴──┘  pages    └──┴──┴──┴──┴──┴──┴──┴──┘
   │  • remove start          │         found by userfaultfd                each segment mapped
   ▼                          ▼         write-protect + PAGEMAP_SCAN        MAP_PRIVATE from the
 Module / InstancePre    base layer L0              │                       newest layer holding it
                         (sparse memfd)             ▼
                              ▲          new layer L2: dirty pages packed   writes go to private
                              │          into a pre-allocated memfd slab    anonymous pages, so
                              │                     │                       siblings never see them
                              └──── L1 ◄──── L2 ◄───┘
                               snapshots form a tree; layers are Arc-shared
                               along it and freed with the last snapshot
```

- **Memory (S1, the default).** Each sandbox's linear memory is our own
  `LinearMemory` (Wasmtime's host-memory hook): a reserved range with
  immutable *layers* mapped copy-on-write on top. A snapshot finds the pages
  written since the last one (async userfaultfd write-protect, read with the
  `PAGEMAP_SCAN` ioctl) and copies only those into a new layer. A spawn maps
  each layer segment `MAP_PRIVATE`. Snapshots compute their mapping plan
  once, and tiered compaction merges layers before chains get deep enough to
  cost many VMAs.
- **Other strategies, for comparison.** S0 copies the whole memory (the test
  oracle). S2 maps nothing at spawn and fills each page on first touch with
  `userfaultfd`. A process-`fork()` baseline forks the whole host.
- **Globals.** A load-time rewrite exports every mutable global
  (`__forkasm_g{N}`), so the host can save and restore `__stack_pointer` and
  friends. It also strips active data segments into the base layer, and
  removes `start` so it only runs at the root.
- **WASI.** `wasi_snapshot_preview1` is implemented in-crate, so every bit of
  state is clonable. Clock and RNG are virtual and seeded per branch: the same
  branch path replays identically.
- **Filesystem.** A persistent copy-on-write tree (`imbl` maps, 64 KiB file
  chunks). Forking clones the root in O(1); writing one byte copies one chunk.
  Host directories (e.g. the CPython stdlib) can be mounted read-only and are
  shared by every branch.
- **Branching.** `Runtime::run_parallel` runs branches on a `rayon` pool, and
  `driver::search` is a beam search (expand N, score, keep k, recurse).

## Demo: eight approaches, one survivor

`forkasm-cli demo` loads CPython 3.12 (WASI), sets up a task (sort 20,000
records) and a checker, forks the interpreter into 8 branches, one per
approach, and runs them in parallel. Six are correct, one gives a wrong answer
and one raises. The fastest correct branch keeps running; the others are
dropped.

```
$ forkasm-cli demo guests/py/out/py.wasm guests/py/out/lib
loaded CPython guest in 132.09 ms
root interpreter + task set up in 189.60 ms
forked 8 branches: snapshot 640.0 µs, spawning all 1.16 ms (7.4 MiB of memory each)

ran all branches in parallel in 334.90 ms

approach                                   time  verdict
builtin sorted                         59.36 ms  correct
list.sort on a copy                    55.77 ms  correct
heapq                                 108.16 ms  correct
merge sort                            334.19 ms  correct
bucket by key, then sort               74.83 ms  correct
cmp_to_key                            320.08 ms  correct
sort by first key only (wrong)                –  wrong answer
typo (raises)                                 –  NameError: name 'sortd' is not defined. Did you mean: 'sorted'?

winner: list.sort on a copy (55.77 ms); the other branches are gone
winner continues: first three = [(0, 0.0021996115705071873), (0, 0.011791623973917642), (0, 0.017203661975841555)]
winner snapshotted again: depth 2, 2 layers, 111 mappings per child
```

## Building and running

```sh
rustup target add wasm32-wasip1
cargo test --release                  # builds guests/kv automatically
guests/py/build.sh                    # CPython guest: downloads wasi-sdk 20 + libpython (checksummed)
cargo run --release -p forkasm-cli -- demo guests/py/out/py.wasm guests/py/out/lib
cargo run --release -p forkasm-cli -- envcheck    # what the kernel supports
```

The benchmark suite (`forkasm-cli bench-suite`) is described in
[`BENCHMARKS.md`](BENCHMARKS.md). Design and decisions: [`SPEC.md`](SPEC.md)
and [`NOTES.md`](NOTES.md).

**Kernel requirements.** S1 needs `memfd_create` and `/proc/self/pagemap`.
The fast path (write-protect tracking, and S2) needs Linux ≥ 6.7 with
`userfaultfd` available to the process: either
`vm.unprivileged_userfaultfd = 1`, or `UFFD_USER_MODE_ONLY`, which forkasm uses
automatically. Without it, S1 falls back to `PAGEMAP_SCAN` or pagemap scans.

## Limitations

- **Only WebAssembly.** Workloads must compile to `wasm32-wasip1`. No
  arbitrary Linux binaries, no threads, no sockets.
- **Fork only between calls.** No mid-call stacks, so guests expose their
  work as exports (`init`, `step`, `py_exec`, …) the host calls one at a time.
- **Module restrictions.** One 32-bit memory; no imported memories, tables
  or mutable globals; no passive data segments; no table mutation. The
  rewrite rejects modules that break these rules.
- **The < 50 µs target is not met.** The default fork is about 2× that (see
  the numbers above). Spawn time grows with the number of mappings, which is
  why compaction exists. On scattered-write workloads a compaction can copy
  all of memory (a ~68 ms p99 snapshot in the adversarial depth benchmark).
- **The VMA limit is process-wide.** Every mapped segment of every live
  sandbox counts against `vm.max_map_count`. Compaction keeps this bounded,
  but very large trees of uncompacted snapshots can still hit it.
- **CPython's `random` keeps its state across forks**, as with `os.fork`;
  reseed in each branch (`os.urandom` and `random.seed()` read the
  per-branch WASI RNG).
- **No security hardening** beyond WebAssembly's own isolation.
- **No merging** branches back together.

## Prior art

forkasm's core idea, restoring WebAssembly execution state through
copy-on-write `mmap`, comes from **Faasm**'s Proto-Faaslets:

> Simon Shillaker and Peter Pietzuch. *Faasm: Lightweight Isolation for
> Efficient Stateful Serverless Computing.* USENIX ATC 2020.
> [paper](https://www.usenix.org/conference/atc20/presentation/shillaker)

Faasm restores a few saved snapshots for serverless cold starts (~500 µs,
~90 kB). forkasm extends the idea to deep, many-way trees of *live* sandboxes,
forks memory and filesystem together, and runs on current Wasmtime.

Other related work: VM-level branching in **forkd** and **Morph Cloud
Infinibranch** (~150–250 ms per branch, as published); Wasmer WASIX
`proc_fork`; Hyperlight-wasm snapshot/restore; Wasmtime's copy-on-write heap
images (start state only); and layered agent filesystems such as AgentFS.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
