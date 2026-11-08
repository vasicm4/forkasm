//! Microsecond live branching of WebAssembly sandboxes.
//!
//! forkasm forks a *running* WebAssembly sandbox (its linear memory, mutable
//! globals, WASI state and filesystem) in about a tenth of a millisecond, many
//! ways and many levels deep. An AI agent, or any search over program states,
//! can branch mid-task, explore alternatives in parallel, and keep the winner.
//!
//! ```no_run
//! use forkasm::{Config, Runtime, SpawnOpts, Val};
//!
//! # fn main() -> anyhow::Result<()> {
//! let rt = Runtime::new(Config::default())?;
//! let module = rt.load("guest.wasm")?; // rewrite + compile once
//! let mut root = rt.spawn(&module, SpawnOpts::default())?;
//! root.call("init", &[])?;
//!
//! let snap = root.snapshot()?; // O(pages dirtied since the last snapshot)
//! let kids = rt.spawn_many(&snap, 8)?; // copy-on-write children
//!
//! let results = rt.run_parallel(kids, |i, sb| sb.call("try", &[Val::I32(i as i32)]));
//! // Keep the best branch; dropping the rest frees their private pages.
//! # Ok(())
//! # }
//! ```
//!
//! # The model
//!
//! - A [`Runtime`] owns the Wasmtime engine and the memory [`Strategy`].
//! - [`Runtime::load`] rewrites a module so all of its state is reachable from
//!   the host (see [`rewrite`]) and compiles it into a [`GuestModule`].
//! - [`Runtime::spawn`] starts a root [`Sandbox`]. You drive it by calling its
//!   exports with [`Sandbox::call`].
//! - Between calls, [`Sandbox::snapshot`] captures it as a [`Snapshot`]: an
//!   immutable, cheaply clonable value that any number of children can be
//!   spawned from ([`Runtime::spawn_from`], [`Runtime::spawn_many`]). Children
//!   are independent and can themselves be snapshotted, forming a tree.
//! - [`Runtime::run_parallel`] runs a closure on many sandboxes at once, and
//!   [`driver::search`] is a ready-made beam search on top.
//!
//! A sandbox can be forked only *between* top-level calls, when no Wasm
//! frames are on the stack, so guests expose their work as exports the host
//! calls one at a time. Each child's WASI clock and RNG are virtual and
//! seeded by its branch index, so the same branch path replays identically.
//!
//! # Requirements
//!
//! Linux on x86-64, single-threaded `wasm32-wasip1` guests with one 32-bit
//! memory. The default strategy needs `memfd_create` and `/proc/self/pagemap`;
//! its fast path needs Linux 6.7 or later with `userfaultfd` available, and
//! falls back to slower dirty-page scans otherwise. `forkasm-cli envcheck`
//! reports what the kernel supports.
//!
//! Benchmarks, the design, and the limitations are in the
//! [repository](https://github.com/vasicm4/forkasm).

#![warn(missing_docs)]

pub mod driver;
pub mod mem;
pub mod python;
pub mod rewrite;
mod runtime;
mod sandbox;
mod snapshot;
pub mod vfs;
pub mod wasi;

pub use runtime::{Compaction, CompileCache, Config, GuestModule, Runtime, SpawnOpts, Strategy};
pub use sandbox::{Sandbox, SandboxStats};
pub use snapshot::Snapshot;
pub use wasmtime::Val;
