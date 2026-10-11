//! forkasm: microsecond live branching of WebAssembly sandboxes.
//!
//! ```ignore
//! let rt = Runtime::new(Config::default())?;
//! let module = rt.load("guest.wasm")?;
//! let mut root = rt.spawn(&module, SpawnOpts::default())?;
//! root.call("init", &[])?;
//! let snap = root.snapshot()?;
//! let kids = rt.spawn_many(&snap, 8)?;
//! ```

pub mod mem;
pub mod rewrite;
mod runtime;
mod sandbox;
mod snapshot;
pub mod wasi;

pub use runtime::{Compaction, Config, GuestModule, Runtime, SpawnOpts, Strategy};
pub use sandbox::{Sandbox, SandboxStats};
pub use snapshot::Snapshot;
pub use wasmtime::Val;
