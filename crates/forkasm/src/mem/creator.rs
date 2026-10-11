//! The `MemoryCreator` that gives every sandbox a forkasm-managed memory.
//!
//! Wasmtime passes no per-instance context to the creator, so a spawn parks
//! the initial contents in a thread-local slot right before instantiating, and
//! the creator picks them up (SPEC §5.2).

use std::cell::RefCell;
use std::sync::Arc;

use wasmtime::{LinearMemory, MemoryCreator, MemoryType};

use super::PAGE;
use super::layer::LayerStack;
use super::mapping::{MemHandle, Region};

/// Initial contents of the next memory created on this thread.
pub(crate) enum MemInit {
    /// Zero-filled, `minimum` bytes.
    Zero,
    /// A private copy of these bytes (strategy S0).
    Copy(Arc<[u8]>),
    /// A composite CoW mapping of these layers (strategy S1), optionally
    /// registered for write tracking.
    Layers(LayerStack, bool),
}

impl MemInit {
    fn byte_size(&self) -> usize {
        match self {
            MemInit::Zero => 0,
            MemInit::Copy(bytes) => bytes.len(),
            MemInit::Layers(stack, _) => stack.byte_size(),
        }
    }
}

thread_local! {
    static NEXT: RefCell<Option<MemInit>> = const { RefCell::new(None) };
    static CREATED: RefCell<Option<MemHandle>> = const { RefCell::new(None) };
}

/// Runs `instantiate` with `init` as the source for the memory it creates and
/// returns its result plus a handle to that memory.
pub(crate) fn instantiate_with<T>(
    init: MemInit,
    instantiate: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<(T, MemHandle)> {
    NEXT.with_borrow_mut(|n| *n = Some(init));
    CREATED.with_borrow_mut(|c| *c = None);
    let result = instantiate();
    NEXT.with_borrow_mut(|n| *n = None);
    let handle = CREATED.with_borrow_mut(Option::take);
    let value = result?;
    let handle = handle.ok_or_else(|| anyhow::anyhow!("module defines no memory"))?;
    Ok((value, handle))
}

/// Default reservation when Wasmtime does not ask for one: 4 GiB, the whole
/// 32-bit address space.
const DEFAULT_RESERVATION: usize = 1 << 32;

pub(crate) struct ForkasmMemoryCreator;

// SAFETY: every memory is a fresh reservation of `reserved + guard` bytes
// (`Region::reserve`) whose first `byte_size` bytes are readable and writable,
// zero-filled unless the spawn asked for specific contents.
unsafe impl MemoryCreator for ForkasmMemoryCreator {
    fn new_memory(
        &self,
        ty: MemoryType,
        minimum: usize,
        maximum: Option<usize>,
        reserved_size_in_bytes: Option<usize>,
        guard_size_in_bytes: usize,
    ) -> Result<Box<dyn LinearMemory>, String> {
        if ty.is_64() || ty.is_shared() {
            return Err("forkasm supports only unshared 32-bit memories".into());
        }
        let init = NEXT.with_borrow_mut(Option::take).unwrap_or(MemInit::Zero);
        let reservation = reserved_size_in_bytes.unwrap_or(DEFAULT_RESERVATION);
        let size = minimum.max(init.byte_size());
        if size > reservation || maximum.is_some_and(|m| size > m) || !size.is_multiple_of(PAGE) {
            return Err(format!("bad initial memory size {size}"));
        }
        let (region, handle) = Region::reserve(reservation, guard_size_in_bytes, maximum)
            .map_err(|e| format!("reserving memory: {e}"))?;
        let init_result = match &init {
            MemInit::Zero => handle.init_zero(size),
            MemInit::Copy(bytes) => handle.init_zero(size).map(|()| handle.write(0, bytes)),
            MemInit::Layers(stack, track) => handle.init_layers(stack, size, *track),
        };
        init_result.map_err(|e| format!("initialising memory: {e}"))?;
        CREATED.with_borrow_mut(|c| *c = Some(handle));
        Ok(Box::new(region))
    }
}
