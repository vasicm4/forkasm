# forkasm — microsecond live branching for agent sandboxes

> Rust · Linux-only · CPU-only · single-threaded guests

## 1. The claim

Fork a **running** WebAssembly sandbox — memory, WASI state and filesystem —
in **microseconds**, many ways and many levels deep, so an AI agent can branch
mid-task, explore alternatives in parallel, and keep the winner.

Targets (to be validated, not assumed):

| Metric | Target |
|---|---|
| Fork latency, 64 MiB memory, < 1% pages dirty since last fork | < 50 µs |
| Extra memory per idle branch | < 64 KiB |
| Fork-of-fork depth supported | ≥ 64 |
| Concurrent live branches | ≥ 1,000 |

The headline comparison: VM-level branching today is ~150–250 ms (forkd,
Morph Infinibranch). forkasm aims to be ~1000× faster for workloads that fit
in WASM.

## 2. Prior art and positioning

| Project | What it does | Gap forkasm targets |
|---|---|---|
| forkd (Rust) | Forks Firecracker microVMs from a warm snapshot via CoW mmap; live BRANCH of a running VM in ~150 ms | VM granularity; live branching is ms-scale |
| Morph Cloud Infinibranch | Commercial microVM snapshot/branch/restore in < 250 ms | Same: VM-scale, hosted, closed |
| agentfork | Forks a live agent's Firecracker sandbox together with its LLM KV cache | VM-scale; focus is KV-cache sharing |
| **Faasm Proto-Faaslets (ATC '20)** | **Snapshots WASM execution state, restores via CoW mmap in ~500 µs / ~90 kB** | **Closest prior art.** A few saved snapshots for serverless cold starts; no deep fork trees, no CoW filesystem, WAVM-based |
| Wasmer WASIX `proc_fork` + journal | POSIX-style fork inside WASM (needs asyncify); journal snapshots to disk | Fork copies memory; snapshots are for persistence |
| Hyperlight-wasm | WASM in micro-VMs with snapshot()/restore() | Reset-to-snapshot, not N-way branching |
| AgentFS (Turso), Rivet agentOS | CoW / layered filesystems for agents | Filesystem layer only |
| Wasmtime CoW heap images, eryx | µs instantiation from a module's *initial* image | Only the start state, not a live, mutated one |

**Positioning:** forkasm extends Faasm's CoW-restore idea to deep, many-way
branch trees of *live* sandboxes, with memory and filesystem forked together,
on modern Wasmtime, aimed at agent tree search. Cite Faasm prominently.

**Honest trade-off:** only WASM-compatible workloads, not arbitrary Linux
binaries.

## 3. Core concepts

- **Sandbox** — one live guest: a Wasmtime `Store` + `Instance`, its linear
  memory, its WASI state (fd table, env, args, clock, RNG) and a VFS root.
- **Snapshot** — an immutable, cheap-to-share capture of a sandbox at a
  quiescent point. Reference-counted.
- **Layer** — an immutable set of memory pages (a sparse `memfd`) that a
  snapshot's memory is composed of. Snapshots form a tree; layers are shared
  along it.
- **Fork** — `snapshot()` + N × `spawn_from(snapshot)`. The parent itself also
  continues on top of the new snapshot.

## 4. Scope decision: fork only at quiescent points

A sandbox may be forked only **between top-level calls** into the guest, when
no Wasm frames are on the native stack. At that point the complete state is:

- linear memory (single memory, 32-bit)
- mutable globals (including `__stack_pointer`, which is back at its base)
- WASI state: fd table and offsets, env, args, clock and RNG state
- the VFS root

No Wasm stack capture, no asyncify. A guest exposes its work as exported
functions (`init`, `step`, `eval`, …) that the host calls one at a time.

## 5. Design

### 5.1 Instantiation path

- Compile each guest once to a `Module`; build an `InstancePre` so spawning
  a child is a cheap instantiate.
- Mutable globals are often **not exported** (e.g. `__stack_pointer`), so the
  host can't read or set them. Fix with a **build-time rewrite** (`walrus` or
  `wasm-encoder`) that exports every mutable global under a reserved prefix
  (`__forkasm_g{N}`). Snapshot = read all; restore = write all.
- Tables: assert they are unchanged since instantiation (no dynamic linking in
  scope). Fail loudly if a guest mutates a table.

### 5.2 Memory: custom `LinearMemory`

- Provide our own linear memory through Wasmtime's host-memory hooks
  (`Config::with_host_memory` + `MemoryCreator` / `LinearMemory`; confirm the
  exact trait signatures against current Wasmtime docs in Week 1).
- Respect Wasmtime's reservation and guard sizes: reserve the full virtual
  range as `PROT_NONE`, map the accessible part on top.
- `MemoryCreator` gets no per-instance context, so a spawn sets a
  "next memory source = snapshot S" slot (thread-local) right before
  instantiation, and the creator reads it.
- **Do not** use `mprotect` + `SIGSEGV` for dirty tracking: Wasmtime owns
  those signal handlers for bounds checks.

### 5.3 Memory: three fork strategies (implement and compare)

**S0 — Naive copy (reference).** Snapshot = `Vec<u8>` copy of memory; spawn =
copy into fresh memory. Correct by construction; the oracle for all tests.

**S1 — Layered CoW mmap (main strategy).**
1. Root sandbox memory is `MAP_PRIVATE` over layer `L0` (a `memfd`, initially
   the module's data image or zeroes).
2. **Dirty detection without clearing anything:** for a `MAP_PRIVATE` file
   mapping, a page the guest has written becomes anonymous. Read
   `/proc/self/pagemap` for the region: present + not `PM_FILE` (bit 61) =
   dirty relative to the layers below. (Faster alternative on Linux ≥ 6.7:
   `PAGEMAP_SCAN` ioctl.)
3. `snapshot()`: create a new sparse `memfd` layer `L_k` and copy **only the
   dirty pages** into it at their offsets. Cost is O(dirty pages), not
   O(memory size).
4. `spawn_from(S)`: map the bottom layer over the whole range, then
   `mmap(MAP_FIXED | MAP_PRIVATE)` each layer's dirty runs on top, oldest to
   newest. Coalesce adjacent pages into runs.
5. The parent remaps itself the same way, so its later writes go to private
   pages and never leak into the snapshot.
6. **Compaction:** each run costs a VMA; Linux caps VMAs per process
   (`vm.max_map_count`, default 65,530) across *all* sandboxes. When a
   snapshot's chain depth or run count passes a threshold, flatten it into one
   full layer. Make thresholds configurable; measure the trade-off.
7. **GC:** layers are `Arc`-shared by snapshots; dropping the last snapshot
   referencing a layer closes its `memfd`.

**S2 — Lazy userfaultfd (stretch, Week 3).** Spawn maps nothing up front;
register the range with `userfaultfd` and fill pages on first touch with
`UFFDIO_COPY` from the right layer. O(1) fork, but every first touch pays a
fault. Requires `UFFD_USER_MODE_ONLY` if unprivileged userfaultfd is disabled.
Compare against S1: fork latency vs. time-to-first-result.

**Baseline B — process fork.** `fork()` the whole host process at a quiescent
point. Free CoW from the kernel; page-table copying makes it scale with RSS.

### 5.4 WASI: own the whole surface

Implement `wasi_snapshot_preview1` ourselves via `Linker::func_wrap` instead of
`wasmtime-wasi`, so every bit of sandbox state is ours to clone.

- Implement what Rust std and CPython need at startup and for files:
  `args_*`, `environ_*`, `fd_prestat_*`, `fd_write`, `fd_read`, `fd_seek`,
  `fd_close`, `fd_fdstat_get`, `fd_filestat_get`, `fd_readdir`, `path_open`,
  `path_filestat_get`, `path_create_directory`, `path_unlink_file`,
  `path_rename`, `clock_time_get`, `random_get`, `proc_exit`.
- Everything else returns `ENOSYS` and is logged once.
- **Deterministic by default:** virtual clock and seeded RNG (seed derived
  from branch id), so a branch replays identically. Real time is opt-in.
- No sockets in scope.

### 5.5 VFS: persistent copy-on-write tree

- Directory = `im::OrdMap<Name, Node>`; file = chunked contents
  (`Vec<Arc<[u8; 64 KiB]>>` or a rope) so writing one byte copies one chunk.
- Fork = clone the root (O(1), structural sharing).
- Optional read-only host mounts (e.g. the CPython stdlib) shared by all
  branches.
- fd table entries point to VFS nodes plus a per-branch offset; the fd table
  is cloned on fork.

### 5.6 Public API (sketch)

```rust
let rt = Runtime::new(Config::default())?;           // strategy, thresholds
let module = rt.load("guest.wasm")?;                 // rewrites + compiles
let mut root = rt.spawn(&module, SpawnOpts::default())?;
root.call("init", &[])?;

let snap = root.snapshot()?;                         // cheap, shareable
let kids: Vec<Sandbox> = rt.spawn_many(&snap, 8)?;   // the fork

let results = rt.run_parallel(kids, |i, sb| sb.call("try", &[(i as i32).into()]))?;
let (winner, _) = results.into_iter().max_by_key(|(_, r)| score(r)).unwrap();
// losers dropped -> their private pages and layers freed
```

Plus `Sandbox::{read_file, write_file, stats}` and `Snapshot::{depth, layers,
bytes}` for tests and benchmarks.

### 5.7 Branch driver

A small tree-search driver on top of the API: expand a node into N branches,
run them on a `rayon` pool, score, keep top-k, recurse. Used for the demo and
the end-to-end benchmark.

## 6. Guest workloads

- **G1 — `guests/kv`:** Rust, `wasm32-wasip1`. In-memory key-value store plus
  file I/O, with exports `init`, `put`, `get`, `touch_pages(n)`,
  `write_file`, `checksum`. `touch_pages` lets benchmarks dirty an exact
  number of pages.
- **G2 — `guests/py`:** CPython for WASI with a tiny C driver exporting
  `py_init()` and `py_exec(ptr, len)` (wraps `PyRun_SimpleString`), so the
  host can fork between statements with variables already defined.
  **Fallback** if the CPython build eats more than a day: QuickJS compiled to
  `wasm32-wasip1` with an exported `js_eval`.

## 7. Correctness plan (tests before features)

- **Oracle:** every strategy is checked against S0 on the same operation
  sequence.
- **Isolation:** writes in a child are invisible to parent and siblings, and
  parent writes after a fork are invisible to children — for memory, globals,
  fd offsets and files.
- **Depth:** fork-of-fork chains to depth 64 equal the oracle byte for byte.
- **Property tests (`proptest`):** random sequences of {call, write, snapshot,
  spawn, drop, compact} across a sandbox tree, compared with S0.
- **Determinism:** the same branch replayed twice gives identical memory
  checksums and stdout.
- **Leaks:** after dropping all sandboxes and snapshots, open `memfd`s, VMAs
  (`/proc/self/maps` line count) and RSS return to baseline.
- **Limits:** a test that approaches the VMA cap and proves compaction keeps
  the process alive.

## 8. Benchmarks

Use `criterion` for micro-benchmarks and a custom harness for multi-branch
runs.

- Fork latency vs. memory size (1 MiB → 1 GiB) at fixed dirty fraction.
- Fork latency vs. dirty pages (1 → 100% of memory) at fixed size.
- Memory per branch at N = 1, 10, 100, 1,000, measured as **PSS** from
  `/proc/self/smaps_rollup` (RSS double-counts shared pages).
- Fork depth vs. latency, with and without compaction.
- S0 vs. S1 vs. S2 vs. process `fork()`.
- End-to-end: driver expanding a tree of G2 branches, total wall time.
- External reference points: forkd and Infinibranch published numbers
  (clearly labelled as theirs, different hardware).

Methodology: warm up, report median and p99 over ≥ 100 runs, record `lscpu`,
kernel version and Wasmtime version in every results file. Cloud VMs are
noisy — rerun headline numbers on your own machine before publishing.

## 9. Repository layout

```
forkasm/
├── SPEC.md  NOTES.md  BENCHMARKS.md  README.md
├── crates/
│   ├── forkasm/            # library
│   │   └── src/
│   │       ├── mem/        # ALL unsafe: memfd, mmap, pagemap, layers, uffd
│   │       ├── wasi/       # preview1 implementation
│   │       ├── vfs/        # persistent CoW tree
│   │       ├── rewrite.rs  # export-mutable-globals pass
│   │       ├── sandbox.rs  snapshot.rs  runtime.rs  driver.rs
│   └── forkasm-cli/        # demo + bench harness
├── guests/  kv/  py/
├── benches/
└── tests/
```

## 10. Milestones and definition of done

**Week 1 (Oct 10–16) — memory fork, S0 and S1 single level**
- `scripts/envcheck.sh`: kernel version, `memfd_create`, `/proc/self/pagemap`
  `PM_FILE` readability, userfaultfd availability, `vm.max_map_count`.
  Record results in `NOTES.md` — they decide S1/S2 details.
- G1 builds; globals-export rewrite works; G1 runs under our WASI subset.
- S0 snapshot/spawn with isolation tests passing.
- S1 with one layer: pagemap dirty detection, sparse layer, composite spawn.
- First criterion benchmark: S0 vs S1 fork latency at 64 MiB.
- **Done when:** `cargo test` green, clippy clean, benchmark numbers in
  `NOTES.md`.

**Week 2 (Oct 17–23) — trees, compaction, filesystem**
- Multi-layer chains, GC, compaction with configurable thresholds.
- Property tests against S0 at depth ≥ 64.
- VFS + WASI file calls; filesystem isolation tests.
- **Done when:** property tests pass 10,000 cases; leak test passes.

**Week 3 (Oct 24–30) — real workload and comparison**
- G2 (CPython, or QuickJS fallback) forking between statements.
- Branch driver + parallel execution.
- S2 (userfaultfd) if time allows; process-`fork()` baseline.
- Full benchmark suite.
- **Done when:** demo runs end to end; all benchmark tables generated.

**Final (Oct 31 – Nov 4) — polish**
- `BENCHMARKS.md` with charts; README with claim, design diagram, honest
  limitations, and Faasm citation.
- Demo: an agent-style script tries 8 approaches to a task in parallel G2
  branches, the winner survives, losers vanish.
- Publish `forkasm 0.1.0` on crates.io.

Credit deadline: Nov 4, 11:59 PM Pacific (morning of Nov 5 in Belgrade).

## 11. Risks and mitigations

| Risk | Mitigation |
|---|---|
| Wasmtime's custom-memory API doesn't fit layered mappings | Prototype in Week 1 day 1; fallback: let Wasmtime own the reservation and remap pages inside it with `MAP_FIXED` from our side |
| `PM_FILE` flag unavailable or misleading in the cloud kernel | `PAGEMAP_SCAN` if kernel ≥ 6.7; else soft-dirty via `clear_refs`; last resort compare-against-layer |
| VMA explosion with many branches | Compaction thresholds; benchmark the cap explicitly |
| CPython WASI build is a time sink | Time-box to 1 day, switch to QuickJS |
| userfaultfd disabled | S2 is a stretch goal; report it as unavailable |
| Cloud VM noise ruins benchmark numbers | Median/p99, repeated runs, confirm on own machine |
| Globals rewrite breaks some modules | Test on G1 and G2 only; document limitation |

## 12. Out of scope

Multi-threaded guests, networking/sockets, multiple memories or memory64,
Windows/macOS, cross-host migration, merging branches back together,
security hardening beyond Wasm isolation, capturing mid-call stacks.

## 13. Rules for Claude Code sessions

- Read `SPEC.md` and `NOTES.md` at the start of every session.
- Work only on the current milestone unless told otherwise.
- Tests first; never mark a milestone done with failing tests.
- Run `cargo test` and `cargo clippy --all-targets -- -D warnings` before
  finishing.
- Log decisions, dead ends and benchmark numbers in `NOTES.md`, newest first.
- Add a dependency only with a one-line justification in `NOTES.md`.
- All `unsafe` lives in `mem/`; every block has a `// SAFETY:` comment.
- If a spec assumption turns out wrong, stop, record it in `NOTES.md`, and
  propose the change instead of silently working around it.

## Appendix: session prompts

**Week 1:**
> Read SPEC.md. Do Week 1 only (section 10). Start with
> scripts/envcheck.sh and record the results in NOTES.md, then prototype the
> custom LinearMemory before anything else, since the S1 design depends on
> it. Then G1, the globals-export rewrite, S0, and S1 single-layer with tests
> and the S0-vs-S1 criterion benchmark. Follow section 13.

**Week 2:**
> Read SPEC.md and NOTES.md. Do Week 2 only: layer chains, GC, compaction,
> property tests vs S0, then the VFS and WASI file calls. Follow section 13.

**Week 3:**
> Read SPEC.md and NOTES.md. Do Week 3: G2 (time-box CPython to one day,
> else QuickJS), the branch driver, baselines, S2 if time allows, and the
> full benchmark suite. Follow section 13.

**Final:**
> Read SPEC.md and NOTES.md. Write BENCHMARKS.md with charts, the README
> (claim, design diagram, limitations, Faasm citation) and the 8-branch demo.
> Prepare the crate for publishing.
